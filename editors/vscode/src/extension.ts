// Shared entry point for BOTH extension hosts (Node desktop + web worker).
// Everything here must stay runtime-neutral: vscode API + fetch only.
import * as vscode from 'vscode';
import { LensClient } from './client';
import {
  associateHostedWorkspace,
  LensEndpointResolver,
  normalizeServerUrl,
  obsoleteServerTokenSecret,
  readHostedAssociation,
  serverTokenSecret,
} from './discovery';
import { LensDiagnostics } from './diagnostics';
import { GraphHoverProvider } from './hover';
import { registerSignalCommands, SignalLensProvider, withdrawLensDocuments } from './lenses';
import { MEMORY_DOC_SCHEME, MemoryCodeLensProvider, MemoryDocProvider, showMemoriesQuickPick } from './memories';
import { logError, logInfo, showLog } from './output';
import { LensOverlays } from './overlays';
import { registerSidebar } from './sidebar';
import { FileStore } from './store';

const RECONNECT_MIN_MS = 1_000;
const RECONNECT_MAX_MS = 30_000;
const WORKSPACE_DOCUMENTS: vscode.DocumentSelector = [{ pattern: '**/*' }];

export function activate(context: vscode.ExtensionContext): void {
  const config = vscode.workspace.getConfiguration('rag-rat-lens');
  const resolver = new LensEndpointResolver(config, context.secrets);
  const client = new LensClient(resolver);
  const store = new FileStore(
    client,
    () => config.get<number>('cloneTheta', 0.9),
    () => config.get<number>('cloneMinTokens', 100),
    (document) => resolver.pathOf(document),
    // Identity, not just address: a restarted local server mints a fresh ownership token, and its
    // index may be a different one than the values already fetched came from.
    async () => {
      const endpoint = await resolver.resolve();
      return `${endpoint.baseUrl} ${endpoint.token ?? ''}`;
    },
  );
  const overlays = new LensOverlays();
  const diagnostics = new LensDiagnostics();
  const codeLens = new MemoryCodeLensProvider(store);
  const memoryDocs = new MemoryDocProvider(store);
  const signalLens = new SignalLensProvider(store);
  registerSignalCommands(context, client);
  const hover = new GraphHoverProvider(store);
  const sidebar = registerSidebar(context, store);

  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 10);
  status.command = 'rag-rat-lens.refresh';
  status.text = '$(pulse) rag-rat: …';
  status.show();

  type StatusProbe =
    | { ok: true; status: Awaited<ReturnType<typeof client.status>> }
    | { ok: false; error: unknown };

  /**
   * Ask the REST surface whether it is answering. This — not the event stream — decides whether
   * data is available: a reverse proxy that buffers or blocks `/api/events` leaves every other
   * endpoint working, and treating that as "offline" would empty every view for a server that is
   * answering fine. A dead stream costs live refresh, which the status bar says out loud.
   *
   * The probe publishes nothing on its own, so a reload that has been superseded while its request
   * was in flight can drop the answer instead of overwriting a newer one.
   */
  async function probeStatus(): Promise<StatusProbe> {
    try {
      return { ok: true, status: await client.status() };
    } catch (error) {
      return { ok: false, error };
    }
  }

  function publishStatus(probe: StatusProbe): void {
    if (!probe.ok) {
      status.text = '$(pulse) rag-rat: offline';
      status.tooltip = 'Lens server unavailable; start `rag-rat mcp` or configure a server URL';
      logError('status', probe.error);
      setDataOnline(false);
      return;
    }
    const head = (probe.status.indexed_head ?? '').slice(0, 8);
    const generation = probe.status.live_files_generation;
    status.text = `$(pulse) rag-rat: ${probe.status.live_file_count} files${eventsOnline ? '' : ' (paused)'}`;
    status.tooltip = eventsOnline
      ? `rag-rat lens — gen ${generation}, head ${head}`
      : `rag-rat lens — gen ${generation}, head ${head}\nLive updates unavailable; click to refresh`;
    setDataOnline(true);
  }

  async function refreshEditor(editor: vscode.TextEditor | undefined): Promise<void> {
    if (!editor) {
      return;
    }
    const path = store.pathOf(editor.document);
    rendered.set(editor.document.uri.toString(), path);
    if (!path) {
      overlays.clear(editor);
      diagnostics.clearUri(editor.document.uri);
      return;
    }
    const loaded = await store.dataFor(editor.document);
    const failure = store.failure(path);
    if (failure) {
      // Present for a partially loaded file too — one lane can fail while the rest answer.
      logError(`overlays ${path}`, failure);
    }
    if (!loaded) {
      // A disconnect must clear hidden editors as they become visible. Epoch invalidation (an SSE
      // refresh racing an in-flight load) stays untouched and resolves on the next request. A
      // document that stopped being indexable mid-load — the buffer went dirty — is cleared
      // outright: `dataFor` withheld the answer precisely so it could not be drawn.
      if (store.shouldClearSignals(path) || store.pathOf(editor.document) !== path) {
        overlays.clear(editor);
        diagnostics.clearUri(editor.document.uri);
      }
      return;
    }
    if (overlays.isEnabled) {
      overlays.apply(editor, loaded.data.clones, loaded.path);
    } else {
      overlays.clear(editor);
    }
    diagnostics.apply(editor.document, loaded.data.memories);
  }

  async function refreshVisibleEditors(): Promise<void> {
    await Promise.all(vscode.window.visibleTextEditors.map((editor) => refreshEditor(editor)));
  }

  /**
   * The indexed path each document was last DRAWN for, `undefined` when its surfaces were
   * withheld. Compared against `pathOf` to decide whether anything needs redrawing.
   *
   * This records what happened, rather than mirroring VS Code's dirty flag. Mirroring means
   * predicting which events imply a transition, and the list is longer than it looks: a save, a
   * revert, an undo back to the saved content, a close that discards edits. Each one missed
   * leaves a document stuck — either showing the saved file's signals over a dirty buffer, or
   * silent forever after a revert. Reading `pathOf` and comparing cannot miss a transition,
   * because it asks the same question the surfaces ask.
   */
  const rendered = new Map<string, string | undefined>();

  /**
   * Redraw everything that speaks for one document after its indexability changed. The CodeLens
   * providers need telling — VS Code caches lenses until a provider fires its change event —
   * while overlays, diagnostics, hovers, and the sidebar follow from `refreshEditor` re-asking
   * `pathOf`.
   */
  async function redrawDocument(document: vscode.TextDocument): Promise<void> {
    const key = document.uri.toString();
    const path = store.pathOf(document);
    // Record FIRST, and whether or not an editor is showing this document. A workspace edit or a
    // rename can dirty a document with no visible editor; leaving the record stale would make
    // every later keystroke re-run this for no effect.
    rendered.set(key, path);
    if (!path) {
      // Diagnostics are owned per URI and shown in the Problems panel whether the document is
      // visible or not, so withholding has to reach them even when there is nothing to draw into.
      diagnostics.clearUri(document.uri);
    }
    refreshViews();
    await Promise.all(
      vscode.window.visibleTextEditors
        .filter((editor) => editor.document.uri.toString() === key)
        .map((editor) => refreshEditor(editor)),
    );
  }

  /** Drop cached index data. Fires no requests — refetching is the pipeline's job, after the gate. */
  function discardIndexData(endpointChanged: boolean): void {
    if (endpointChanged) {
      store.reset();
    } else {
      store.invalidate();
    }
  }

  /**
   * Re-drive EVERY surface that reads index data. This list is the whole set: the tree views, the
   * CodeLens providers, the open `rag-rat-memory:` documents, and the visible editors' overlays
   * and diagnostics.
   *
   * It exists so surfaces cannot end up on different sides of the reachability gate. They used to
   * be driven from two places — some before the status probe and some after — and a surface
   * refreshed before it is pointless: the store refuses every lane while offline, so it reads
   * `undefined` and keeps whatever it was already showing. `refreshViews` happened to recover
   * because `setDataOnline` calls it again; the memory documents had no second caller, so an
   * offline-to-online transition left them on their pre-outage content until the next version
   * event. Add new surfaces HERE, not to a caller.
   */
  async function refreshIndexSurfaces(): Promise<void> {
    refreshViews();
    await Promise.all([memoryDocs.refresh(), refreshVisibleEditors()]);
  }

  /**
   * Everything that reads index data: the file-data cache, the views over it, the memory
   * documents, the status bar, and the signals on open documents. Every "the index moved under us"
   * path goes through this one call so no site can refresh a subset and leave a stale surface
   * behind — the status bar's file count is the one that drifted that way.
   *
   * Only visible editors are re-fetched (each file costs five requests, and an index change is a
   * per-save event), so a hidden document's diagnostics are DROPPED rather than refreshed: they
   * would otherwise keep asserting a memory verdict from an index state that no longer exists,
   * with nothing to correct them until that tab is next selected.
   *
   * The status probe runs FIRST because it is what publishes reachability: the file lanes are
   * refused while the store is offline, so fetching them concurrently would race the answer.
   *
   * Reloads overlap — a stream failure starts one, then the stream reconnects and starts another —
   * so only the newest may publish. A superseded probe that rejects afterwards would otherwise
   * take a reachable server offline and clear what the newer one just loaded, with no further
   * event coming to undo it.
   */
  async function reloadIndexData(options: { endpointChanged?: boolean } = {}): Promise<boolean> {
    const generation = ++reloadGeneration;
    discardIndexData(options.endpointChanged === true);
    // Repaint the now-empty views immediately so a slow probe does not leave stale content on
    // screen; the real refetch happens below, once reachability is published.
    refreshViews();
    diagnostics.retainOnly(vscode.window.visibleTextEditors.map((editor) => editor.document.uri));
    const probe = await probeStatus();
    if (generation !== reloadGeneration) {
      return probe.ok;
    }
    publishStatus(probe);
    if (!probe.ok) {
      clearSignals();
      return false;
    }
    // Symmetric with `clearSignals()` on the failure path: the server is answering, so memory
    // documents may render real content again. It has to happen HERE, where reachability is
    // actually known, and not as a side effect inside the refresh — with no memory documents open
    // that refresh iterates nothing, and a withdrawal made while none were open would never lift.
    memoryDocs.restore();
    await refreshIndexSurfaces();
    return true;
  }

  function refreshViews(): void {
    sidebar.refresh();
    codeLens.refresh();
    signalLens.refresh();
  }

  function setDataOnline(online: boolean): void {
    store.setOnline(online);
    refreshViews();
  }

  /** Offline or re-pointed: no open document's signals are trustworthy, visible or not. */
  function clearSignals(): void {
    for (const editor of vscode.window.visibleTextEditors) {
      overlays.clear(editor);
    }
    diagnostics.clear();
    // Open documents are signals too. They are the surfaces where staleness leaves no trace — a
    // decoration on a moved line looks wrong, a rendered memory body or extraction proposal does
    // not — so they are withdrawn on the same path rather than left showing an unreachable
    // server's claim. `rag-rat-doc:` documents especially: nothing ever refetches them, because
    // the command that produced them has already returned.
    memoryDocs.withdraw();
    withdrawLensDocuments();
  }

  let streamController = new AbortController();
  let lastVersion = '';
  let eventsOnline = false;
  let eventFailureLogged = false;
  let reloadGeneration = 0;
  async function watchVersions(signal: AbortSignal): Promise<void> {
    let reconnectMs = RECONNECT_MIN_MS;
    while (!signal.aborted) {
      try {
        await client.watchVersions(signal, (version) => {
          reconnectMs = RECONNECT_MIN_MS;
          const reconnected = !eventsOnline;
          if (!eventsOnline) {
            eventsOnline = true;
            eventFailureLogged = false;
            void reloadIndexData();
          }
          const token = JSON.stringify(version);
          if (!reconnected && lastVersion && token !== lastVersion) {
            void reloadIndexData();
          }
          lastVersion = token;
        });
      } catch (error) {
        if (!signal.aborted) {
          eventsOnline = false;
          resolver.invalidate();
          // A dead stream is not a dead server. Reload through the status probe: if REST answers,
          // the data stays usable and the status bar reports that refresh is manual until the
          // stream returns; if it does not, the probe is what takes everything offline.
          void reloadIndexData();
          if (!eventFailureLogged) {
            logError('events', error);
            eventFailureLogged = true;
          }
          await delay(reconnectMs, signal);
          reconnectMs = Math.min(reconnectMs * 2, RECONNECT_MAX_MS);
        }
      }
    }
  }

  /**
   * Re-point the event stream. Signals go with it because every caller has just changed WHICH
   * server answers, but reachability is left to the status probe in the reload that follows —
   * declaring the store offline here would make the manual refresh command unable to load anything
   * until the stream happened to connect.
   */
  function restartEvents(): void {
    streamController.abort();
    streamController = new AbortController();
    lastVersion = '';
    eventsOnline = false;
    clearSignals();
    void watchVersions(streamController.signal);
  }

  context.subscriptions.push(
    status,
    overlays,
    diagnostics,
    codeLens,
    signalLens,
    vscode.commands.registerCommand('rag-rat-lens.toggleOverlays', async () => {
      const on = overlays.toggle();
      void vscode.window.showInformationMessage(`rag-rat lens overlays ${on ? 'on' : 'off'}`);
      await refreshVisibleEditors();
    }),
    vscode.commands.registerCommand('rag-rat-lens.refresh', async () => {
      // Re-resolving discovery can land on a different server, so nothing previously fetched is
      // carried over — this is also the user's escape hatch when something looks wrong.
      resolver.invalidate();
      restartEvents();
      await reloadIndexData({ endpointChanged: true });
    }),
    vscode.commands.registerCommand('rag-rat-lens.configureServer', async () => {
      const previousUrl = config.inspect<string>('serverUrl')?.globalValue?.trim() ?? '';
      const serverUrl = await vscode.window.showInputBox({
        prompt: 'Hosted rag-rat Lens server URL (leave empty to use workspace discovery)',
        value: previousUrl,
        ignoreFocusOut: true,
      });
      if (serverUrl === undefined) {
        return;
      }
      const token = serverUrl.trim()
        ? await vscode.window.showInputBox({
            prompt: 'Bearer token',
            password: true,
            ignoreFocusOut: true,
          })
        : '';
      if (token === undefined) {
        return;
      }
      let normalizedUrl = '';
      try {
        normalizedUrl = serverUrl.trim() ? normalizeServerUrl(serverUrl) : '';
      } catch (error) {
        void vscode.window.showErrorMessage(
          `Invalid Lens server URL: ${error instanceof Error ? error.message : String(error)}`,
        );
        return;
      }
      if (normalizedUrl) {
        // The override describes how this workspace sits inside this server's index, so it is
        // asked for — and stored — per association instead of once per machine.
        // Prefill only: an unreadable association is reported by the association call below.
        const previousRoot = await readHostedAssociation(normalizedUrl, context.secrets)
          .then((association) => association?.indexedRoot)
          .catch(() => undefined);
        const indexedRoot = await vscode.window.showInputBox({
          prompt:
            'Indexed root relative to this workspace (empty to use server metadata; "." when the opened folder is already the indexed root)',
          value: previousRoot ?? '',
          ignoreFocusOut: true,
        });
        if (indexedRoot === undefined) {
          return;
        }
        try {
          await associateHostedWorkspace(
            normalizedUrl,
            token.trim() || undefined,
            context.secrets,
            indexedRoot.trim() || undefined,
          );
        } catch (error) {
          void vscode.window.showErrorMessage(
            `Could not associate Lens server with this workspace: ${error instanceof Error ? error.message : String(error)}`,
          );
          return;
        }
        const secretKey = serverTokenSecret(normalizedUrl);
        if (token.trim()) {
          await context.secrets.store(secretKey, token.trim());
        } else {
          await context.secrets.delete(secretKey);
        }
      }
      await config.update('serverUrl', normalizedUrl, vscode.ConfigurationTarget.Global);
      // Never leave the previous origin's credential behind after a URL change — a stale secret
      // must not follow the extension to a new server.
      const obsoleteSecret = obsoleteServerTokenSecret(previousUrl, normalizedUrl);
      if (obsoleteSecret) {
        await context.secrets.delete(obsoleteSecret);
      }
      resolver.invalidate();
      restartEvents();
      await reloadIndexData({ endpointChanged: true });
    }),
    vscode.commands.registerCommand(
      'rag-rat-lens.openLocation',
      async ({ path, line }: { path: string; line: number }) => {
        const uri = isSafeRepoPath(path) ? resolver.uriOf(path) : undefined;
        if (!uri) {
          return;
        }
        const doc = await vscode.workspace.openTextDocument(uri);
        const editor = await vscode.window.showTextDocument(doc);
        const pos = new vscode.Position(Math.max(0, line - 1), 0);
        editor.selection = new vscode.Selection(pos, pos);
        editor.revealRange(new vscode.Range(pos, pos), vscode.TextEditorRevealType.InCenter);
      },
    ),
    vscode.commands.registerCommand('rag-rat-lens.showLog', () => showLog()),
    vscode.commands.registerCommand('rag-rat-lens.showMemories', (
      memories: Parameters<typeof showMemoriesQuickPick>[0],
      path: Parameters<typeof showMemoriesQuickPick>[1],
    ) =>
      showMemoriesQuickPick(memories, path, memoryDocs),
    ),
    vscode.languages.registerCodeLensProvider(WORKSPACE_DOCUMENTS, codeLens),
    vscode.languages.registerCodeLensProvider(WORKSPACE_DOCUMENTS, signalLens),
    vscode.languages.registerHoverProvider(WORKSPACE_DOCUMENTS, hover),
    vscode.workspace.registerTextDocumentContentProvider(MEMORY_DOC_SCHEME, memoryDocs),
    memoryDocs,
    vscode.window.onDidChangeActiveTextEditor((editor) => refreshEditor(editor)),
    // Dirtiness decides whether a document has an indexed path at all, so a change that flips it
    // has to redraw. Only the TRANSITION does work: a keystroke in an already-dirty buffer changes
    // nothing about what may be shown, and redrawing per keystroke would clear and rebuild every
    // decoration in the file for no reason. Reverting is a change too, so restoring the saved
    // content restores its surfaces through this same path.
    vscode.workspace.onDidChangeTextDocument((event) => {
      const key = event.document.uri.toString();
      if (rendered.has(key) && store.pathOf(event.document) !== rendered.get(key)) {
        void redrawDocument(event.document);
      }
    }),
    // Saving is deliberately NOT special-cased. The buffer is clean again, so the dirty gate
    // reopens and the file's signals return from whatever the store last fetched — which may still
    // describe the pre-save contents until the watcher reindexes and the version event repaints.
    // Withholding across that window was tried and removed: no index-wide signal proves that THIS
    // file was reindexed, so every variant either released too early or, when a fast watcher's
    // version event beat the save callback, left the file silent forever. A bounded, self-
    // correcting staleness window is the better failure. Closing it needs the server to report the
    // content its answer was computed from — see #1021.
    vscode.workspace.onDidCloseTextDocument((doc) => {
      diagnostics.clearUri(doc.uri);
      // What was drawn for a closed document is not a fact about the next one opened at that URI —
      // closing a dirty buffer discards the edits, so it reopens clean. Dropping it also keeps the
      // map from growing across a long session.
      rendered.delete(doc.uri.toString());
      if (doc.uri.scheme === MEMORY_DOC_SCHEME) {
        memoryDocs.forget(doc.uri);
      }
    }),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration('rag-rat-lens')) {
        resolver.invalidate();
        restartEvents();
        void reloadIndexData({ endpointChanged: true });
      }
    }),
    // A replaced folder is a different repository even when the server URL is identical, so every
    // cached path mapping and payload has to go. The resolver's caches are workspace-keyed and
    // would refuse to serve the old mapping anyway; this is what makes the surfaces reflect it
    // now rather than at the next version event.
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      resolver.invalidate();
      restartEvents();
      void reloadIndexData({ endpointChanged: true });
    }),
    { dispose: () => streamController.abort() },
  );

  void reloadIndexData().then((ok) =>
    logInfo(ok ? 'lens server connected' : 'lens server offline at startup')
  );
  void watchVersions(streamController.signal);
}

export function deactivate(): void {}

function delay(ms: number, signal: AbortSignal): Promise<void> {
  if (signal.aborted) {
    return Promise.resolve();
  }
  return new Promise((resolve) => {
    const onAbort = () => {
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(() => {
      signal.removeEventListener('abort', onAbort);
      resolve();
    }, ms);
    signal.addEventListener('abort', onAbort, { once: true });
  });
}

// Server responses are repository-intelligence data, not filesystem authority: a hosted server
// must never steer the extension outside the workspace root.
function isSafeRepoPath(path: string): boolean {
  if (!path || path.startsWith('/') || /^[A-Za-z]:[\\/]/.test(path)) {
    return false;
  }
  return !path.split(/[\\/]+/).some((segment) => segment === '..');
}
