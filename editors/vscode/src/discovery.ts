import * as vscode from 'vscode';
import {
  indexedPath,
  indexedRootPrefixForDiscovery,
  normalizeRelativePath,
  workspacePath,
} from './workspace_paths';

export interface LensEndpoint {
  baseUrl: string;
  token?: string;
}

interface LensDiscovery {
  schema: string;
  version: number;
  url: string;
  indexed_root: string;
  case_insensitive_paths: boolean;
  ownership_token: string;
}

interface LensServerStatus {
  repo_id: string;
  /** The checkout the server indexes — empty for a main worktree, the linked worktree otherwise. */
  worktree_id: string;
  indexed_root: string;
  case_insensitive_paths: boolean;
}

interface DiscoveryCandidate {
  uri: vscode.Uri;
  workspacePath: string;
}

/**
 * What "rag-rat Lens: Configure Server" recorded for one (server, workspace) pair. The indexed-root
 * override lives here rather than in a machine-wide setting: it describes how *this* workspace sits
 * inside *that* server's index, so a second workspace or endpoint must not inherit it.
 */
interface HostedAssociation {
  repoId: string;
  /**
   * The CHECKOUT that was bound, not just the repository. A repository's linked worktrees share
   * one `repo_id`, so `repoId` alone would keep accepting a server that has moved to a different
   * checkout — and its memories, clones, and graph data are anchored to that checkout's lines.
   */
  worktreeId: string;
  indexedRoot?: string;
}

const DISCOVERY_TTL_MS = 2_000;

/** A discovery file pointing off-loopback is a hard boundary violation, never "try next folder". */
class NonLoopbackDiscoveryError extends Error {}

/**
 * Which folders this resolution describes. Every cached resolution is keyed by it, because a
 * resolution is a statement about a WORKSPACE and a server, never about a server alone: a
 * single-root window can replace its folder without reloading the extension host, and a cache
 * keyed only on the base URL would keep serving the previous folder's indexed-root and checkout
 * mapping — painting one repository's signals over the new one's same-named files.
 */
function workspaceKey(): string {
  return (vscode.workspace.workspaceFolders ?? [])
    .map((folder) => folder.uri.toString())
    .join(' ');
}

interface ConfiguredMapping {
  baseUrl: string;
  workspace: string;
  indexedRootPrefix: string;
  caseInsensitivePaths: boolean;
}

/**
 * One completed resolution, returned rather than written straight to the resolver's fields. The
 * path mapping used to be installed as a side effect partway through resolving, which left no
 * moment at which it could be checked against the workspace it describes.
 */
interface Resolution {
  endpoint: LensEndpoint;
  indexedRootPrefix: string;
  caseInsensitivePaths: boolean;
  configuredMapping: ConfiguredMapping | undefined;
}

export class LensEndpointResolver {
  private cached: { endpoint: LensEndpoint; at: number; workspace: string } | undefined;
  private configuredMapping: ConfiguredMapping | undefined;
  private indexedRootPrefix = '';
  private caseInsensitivePaths = false;
  /** Bumped by `invalidate()` so a resolution started before it cannot publish after it. */
  private generation = 0;

  constructor(
    private readonly config: vscode.WorkspaceConfiguration,
    private readonly secrets: vscode.SecretStorage,
  ) {}

  invalidate(): void {
    this.generation += 1;
    this.cached = undefined;
    this.configuredMapping = undefined;
  }

  async resolve(): Promise<LensEndpoint> {
    const workspace = workspaceKey();
    const generation = this.generation;
    if (
      this.cached &&
      this.cached.workspace === workspace &&
      Date.now() - this.cached.at < DISCOVERY_TTL_MS
    ) {
      return this.cached.endpoint;
    }
    const resolved = await this.resolveUncached();
    // Resolving awaits filesystem and network I/O, and the world can move under it: a folder can
    // be replaced, or a settings change can call `invalidate()`. Publishing then would install the
    // PREVIOUS workspace's indexed-root mapping over the current one, and every path computed
    // from it afterwards would name a file in the wrong repository. Only the resolution that
    // still describes where we are may publish; the caller retries, and the folder/settings change
    // that superseded this one triggers its own reload anyway.
    if (this.generation !== generation || workspaceKey() !== workspace) {
      throw new Error('Lens server resolution was superseded by a workspace or settings change');
    }
    this.indexedRootPrefix = resolved.indexedRootPrefix;
    this.caseInsensitivePaths = resolved.caseInsensitivePaths;
    this.configuredMapping = resolved.configuredMapping;
    this.cached = { endpoint: resolved.endpoint, at: Date.now(), workspace };
    return resolved.endpoint;
  }

  private async resolveUncached(): Promise<Resolution> {
    requireSingleRootWorkspace();
    // Only the user-level value may select a remote host: a checked-out repository must never be
    // able to redirect the SecretStorage bearer token through workspace settings.
    const configuredUrl = this.config.inspect<string>('serverUrl')?.globalValue?.trim() ?? '';
    if (configuredUrl) {
      const baseUrl = normalizeServerUrl(configuredUrl);
      const token = (await this.secrets.get(serverTokenSecret(baseUrl)))?.trim() || undefined;
      // Reuse only when BOTH halves match. The association is stored per (server, workspace), so a
      // mapping cached under one workspace says nothing about the next one's indexed root.
      const workspace = workspaceKey();
      const mapping =
        this.configuredMapping?.baseUrl === baseUrl &&
        this.configuredMapping.workspace === workspace
          ? this.configuredMapping
          : {
              workspace,
              ...(await configuredWorkspaceMapping(this.secrets, baseUrl, token)),
            };
      return {
        endpoint: { baseUrl, token },
        indexedRootPrefix: mapping.indexedRootPrefix,
        caseInsensitivePaths: mapping.caseInsensitivePaths,
        configuredMapping: mapping,
      };
    }
    if (!vscode.workspace.isTrusted) {
      throw new Error('workspace trust is required for automatic Lens server discovery');
    }

    for (const folder of discoveryFolders()) {
      // On web/virtual workspaces the fs is virtual, so a committed
      // `.rag-rat/sockets/lens.json` would be repository content — an attacker-controlled
      // redirect target for a browser-issued loopback request. Web hosts must use the
      // explicitly configured global server URL instead.
      if (folder.uri.scheme !== 'file') {
        continue;
      }
      for (const candidate of await discoveryCandidates(folder)) {
        const uri = vscode.Uri.joinPath(candidate.uri, '.rag-rat', 'sockets', 'lens.json');
        try {
          const bytes = await vscode.workspace.fs.readFile(uri);
          const discovery = JSON.parse(new TextDecoder().decode(bytes)) as Partial<LensDiscovery>;
          if (
            discovery.schema !== 'rag-rat-lens-discovery' ||
            discovery.version !== 1 ||
            typeof discovery.url !== 'string' ||
            typeof discovery.indexed_root !== 'string' ||
            typeof discovery.case_insensitive_paths !== 'boolean' ||
            typeof discovery.ownership_token !== 'string' ||
            !discovery.ownership_token
          ) {
            continue;
          }
          const prefix = indexedRootPrefixForDiscovery(
            discovery.indexed_root,
            candidate.workspacePath,
            discovery.case_insensitive_paths,
          );
          if (prefix === undefined) {
            continue;
          }
          const baseUrl = normalizeServerUrl(discovery.url);
          const host = new URL(baseUrl).hostname;
          if (!isLoopbackHost(host)) {
            throw new NonLoopbackDiscoveryError(
              `refusing non-loopback Lens discovery URL ${baseUrl}`,
            );
          }
          return {
            endpoint: { baseUrl, token: discovery.ownership_token },
            indexedRootPrefix: prefix,
            caseInsensitivePaths: discovery.case_insensitive_paths,
            // Automatic discovery carries no hosted association to remember.
            configuredMapping: undefined,
          };
        } catch (error) {
          if (error instanceof NonLoopbackDiscoveryError) {
            throw error;
          }
        }
      }
    }
    throw new Error(
      'no Lens discovery file found; start `rag-rat mcp` in a local workspace or run "rag-rat Lens: Configure Server"',
    );
  }

  /**
   * The indexed path whose CONTENT this document currently shows, or `undefined` when there is
   * none to speak for.
   *
   * Every per-document surface — overlays, diagnostics, hovers, both CodeLens providers, and the
   * sidebar's active-file view — asks this one question and renders nothing when the answer is
   * `undefined`. That makes it the single gate for "may line-anchored index data be shown here",
   * which is why the unsaved-buffer check belongs here and not in each of them.
   *
   * A dirty buffer has no answer. Every fact the server can offer for it — a clone region, a
   * memory binding, a coupling partner — is anchored to line numbers in the SAVED file, and one
   * inserted line above puts every one of them on the wrong statement. Worse, an edit can delete
   * the very code a warning is about while the warning stays on screen. The index has never seen
   * these bytes, so the honest answer is silence until the file is saved and reindexed.
   */
  pathOf(document: vscode.TextDocument): string | undefined {
    if (
      vscode.workspace.workspaceFolders?.length !== 1 ||
      !vscode.workspace.getWorkspaceFolder(document.uri) ||
      document.isDirty
    ) {
      return undefined;
    }
    return indexedPath(
      vscode.workspace.asRelativePath(document.uri, false),
      this.indexedRootPrefix,
      this.caseInsensitivePaths,
    );
  }

  uriOf(path: string): vscode.Uri | undefined {
    const folder = vscode.workspace.workspaceFolders?.length === 1
      ? vscode.workspace.workspaceFolders[0]
      : undefined;
    const relative = workspacePath(path, this.indexedRootPrefix);
    if (!folder || relative === undefined) {
      return undefined;
    }
    return vscode.Uri.joinPath(folder.uri, ...relative.split('/'));
  }
}

function discoveryFolders(): readonly vscode.WorkspaceFolder[] {
  return vscode.workspace.workspaceFolders ?? [];
}

async function discoveryCandidates(folder: vscode.WorkspaceFolder): Promise<DiscoveryCandidate[]> {
  const candidates: DiscoveryCandidate[] = [{ uri: folder.uri, workspacePath: '' }];
  const workspaceSegments: string[] = [];
  let uri = folder.uri;
  while (true) {
    if (await exists(vscode.Uri.joinPath(uri, '.git'))) {
      if (uri.toString() !== folder.uri.toString()) {
        candidates.push({ uri, workspacePath: workspaceSegments.join('/') });
      }
      break;
    }
    const segment = uri.path.split('/').filter(Boolean).at(-1);
    const parent = vscode.Uri.joinPath(uri, '..');
    if (!segment || parent.toString() === uri.toString()) {
      break;
    }
    workspaceSegments.unshift(segment);
    uri = parent;
  }
  return candidates;
}

async function exists(uri: vscode.Uri): Promise<boolean> {
  try {
    await vscode.workspace.fs.stat(uri);
    return true;
  } catch {
    return false;
  }
}

function requireSingleRootWorkspace(): void {
  const folders = vscode.workspace.workspaceFolders ?? [];
  if (folders.length !== 1) {
    throw new Error('rag-rat Lens currently requires a single-root workspace');
  }
}

export function normalizeServerUrl(raw: string): string {
  const url = new URL(raw);
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new Error('Lens server URL must use http or https');
  }
  if (url.protocol === 'http:' && !isLoopbackHost(url.hostname)) {
    throw new Error('non-loopback Lens server URLs must use https');
  }
  if (url.pathname !== '/' || url.search || url.hash) {
    throw new Error('Lens server URL must not include a path, query, or fragment');
  }
  // `url.origin` would silently drop embedded credentials, leaving the extension to call an
  // endpoint the user believes is authenticated by them. Say so instead — the server authenticates
  // with the bearer token this command stores, which is also why it rejects credentials in its own
  // origin allowlist.
  if (url.username || url.password) {
    throw new Error('Lens server URL must not embed credentials; the bearer token authenticates');
  }
  return url.origin;
}

export function serverTokenSecret(baseUrl: string): string {
  return `rag-rat-lens.serverToken:${encodeURIComponent(normalizeServerUrl(baseUrl))}`;
}

export function obsoleteServerTokenSecret(
  previousUrl: string,
  normalizedUrl: string,
): string | undefined {
  if (!previousUrl) {
    return undefined;
  }
  const previousKey = serverTokenSecret(previousUrl);
  const nextKey = normalizedUrl ? serverTokenSecret(normalizedUrl) : undefined;
  return previousKey === nextKey ? undefined : previousKey;
}

async function configuredWorkspaceMapping(
  secrets: vscode.SecretStorage,
  baseUrl: string,
  token: string | undefined,
): Promise<{ baseUrl: string; indexedRootPrefix: string; caseInsensitivePaths: boolean }> {
  const status = await fetchServerStatus(baseUrl, token);
  const association = await readHostedAssociation(baseUrl, secrets);
  if (!association) {
    throw new Error(
      'hosted Lens server is not associated with this workspace; run "rag-rat Lens: Configure Server"',
    );
  }
  // Both halves, because a repository's linked worktrees share one `repo_id`: a server that has
  // moved to another checkout still answers with the repository this workspace was bound to, while
  // every line it reports belongs to a different working tree.
  if (association.repoId !== status.repo_id || association.worktreeId !== status.worktree_id) {
    throw new Error(
      'hosted Lens server no longer serves the checkout this workspace was associated with; run "rag-rat Lens: Configure Server"',
    );
  }
  const indexedRootPrefix = normalizeRelativePath(association.indexedRoot ?? status.indexed_root);
  if (indexedRootPrefix === undefined) {
    throw new Error('Lens indexed root must be a safe workspace-relative path');
  }
  return {
    baseUrl,
    indexedRootPrefix,
    caseInsensitivePaths: status.case_insensitive_paths,
  };
}

async function fetchServerStatus(
  baseUrl: string,
  token: string | undefined,
): Promise<LensServerStatus> {
  const response = await fetch(new URL('/api/status', baseUrl), {
    headers: token ? { Authorization: `Bearer ${token}` } : undefined,
    signal: AbortSignal.timeout(10_000),
  });
  if (!response.ok) {
    throw new Error(`/api/status -> ${response.status}`);
  }
  const status = (await response.json()) as Partial<LensServerStatus>;
  if (
    typeof status.repo_id !== 'string' ||
    !status.repo_id ||
    typeof status.worktree_id !== 'string' ||
    typeof status.indexed_root !== 'string' ||
    typeof status.case_insensitive_paths !== 'boolean'
  ) {
    throw new Error('/api/status returned invalid workspace metadata');
  }
  return status as LensServerStatus;
}

function workspaceIdentity(): string {
  requireSingleRootWorkspace();
  return vscode.workspace.workspaceFolders![0].uri.toString();
}

export function hostedRepoAssociationSecret(baseUrl: string, workspaceUri: string): string {
  return `rag-rat-lens.serverRepo:${encodeURIComponent(normalizeServerUrl(baseUrl))}:${encodeURIComponent(workspaceUri)}`;
}

/** Read this workspace's association with `baseUrl`; an unreadable record fails closed as absent. */
export async function readHostedAssociation(
  baseUrl: string,
  secrets: vscode.SecretStorage,
): Promise<HostedAssociation | undefined> {
  const raw = await secrets.get(hostedRepoAssociationSecret(baseUrl, workspaceIdentity()));
  if (!raw) {
    return undefined;
  }
  let parsed: Partial<HostedAssociation>;
  try {
    parsed = JSON.parse(raw) as Partial<HostedAssociation>;
  } catch {
    return undefined;
  }
  if (typeof parsed.repoId !== 'string' || !parsed.repoId) {
    return undefined;
  }
  // A record without the checkout half predates that binding and cannot be checked against the
  // server; fail closed and let the user re-associate rather than trust the weaker claim.
  if (typeof parsed.worktreeId !== 'string') {
    return undefined;
  }
  if (parsed.indexedRoot !== undefined && typeof parsed.indexedRoot !== 'string') {
    return undefined;
  }
  return {
    repoId: parsed.repoId,
    worktreeId: parsed.worktreeId,
    indexedRoot: parsed.indexedRoot,
  };
}

export async function associateHostedWorkspace(
  baseUrl: string,
  token: string | undefined,
  secrets: vscode.SecretStorage,
  indexedRootOverride?: string,
): Promise<void> {
  const status = await fetchServerStatus(baseUrl, token);
  if (
    indexedRootOverride !== undefined &&
    normalizeRelativePath(indexedRootOverride) === undefined
  ) {
    throw new Error('Lens indexed root must be a safe workspace-relative path');
  }
  const association: HostedAssociation = {
    repoId: status.repo_id,
    worktreeId: status.worktree_id,
    indexedRoot: indexedRootOverride,
  };
  await secrets.store(
    hostedRepoAssociationSecret(baseUrl, workspaceIdentity()),
    JSON.stringify(association),
  );
}

export function isLoopbackHost(host: string): boolean {
  if (host === 'localhost' || host === '[::1]') {
    return true;
  }
  const octets = host.split('.');
  return octets.length === 4
    && octets[0] === '127'
    && octets.every((part) => /^\d+$/.test(part) && Number(part) <= 255);
}
