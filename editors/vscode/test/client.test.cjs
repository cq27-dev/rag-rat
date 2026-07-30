const assert = require('node:assert/strict');
const http = require('node:http');
const path = require('node:path');
const test = require('node:test');

const esbuild = require('esbuild');

async function loadClientModule() {
  const result = await esbuild.build({
    entryPoints: [path.join(__dirname, '..', 'src', 'client.ts')],
    bundle: true,
    format: 'cjs',
    platform: 'node',
    write: false,
  });
  const module = { exports: {} };
  new Function('module', 'exports', 'require', result.outputFiles[0].text)(
    module,
    module.exports,
    require,
  );
  return module.exports;
}

async function loadClient() {
  return (await loadClientModule()).LensClient;
}

async function loadStore() {
  const result = await esbuild.build({
    entryPoints: [path.join(__dirname, '..', 'src', 'store.ts')],
    bundle: true,
    external: ['vscode'],
    format: 'cjs',
    platform: 'node',
    write: false,
  });
  const module = { exports: {} };
  new Function('module', 'exports', 'require', result.outputFiles[0].text)(
    module,
    module.exports,
    (specifier) => (specifier === 'vscode' ? {} : require(specifier)),
  );
  return module.exports.FileStore;
}

async function loadSourceModule(file, mockVscode) {
  const result = await esbuild.build({
    entryPoints: [path.join(__dirname, '..', 'src', file)],
    bundle: true,
    external: mockVscode ? ['vscode'] : [],
    format: 'cjs',
    platform: 'node',
    write: false,
  });
  const module = { exports: {} };
  new Function('module', 'exports', 'require', result.outputFiles[0].text)(
    module,
    module.exports,
    (specifier) => (specifier === 'vscode' ? mockVscode : require(specifier)),
  );
  return module.exports;
}

/** The store keys its per-lane fallbacks by which server answered; one stable identity by default. */
const endpoint = async () => 'http://127.0.0.1:18120 owner-token';

function deferred() {
  let resolve;
  const promise = new Promise((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

test('clone payload validation tolerates nullable and unknown fields', async () => {
  const { validateFileClones } = await loadClientModule();
  // The Rust contract shape: nullable max_similarity, unknown refine JSON, null partner symbol.
  const payload = {
    clone_regions: [
      {
        start_line: 1,
        end_line: 3,
        byte_offset: 0,
        class_id: 7,
        class_key: 'deadbeefcafebabe',
        symbol: 'load_user',
        max_similarity: null,
        partners: [
          { path: 'src/b.rs', start_line: 1, end_line: 3, similarity: 0.91, symbol: null },
        ],
        refine: {
          template: 'fn extracted() { ⟨m0⟩ }',
          variation_points: [],
          proposed_signature: null,
          confidence: 'high',
          anti_unify_coverage: 1,
          lcs_ratio: 1,
          refactorability: 1,
        },
      },
    ],
    clone_graph: { generation: 3, eligible: true, stale: false },
  };
  const validated = validateFileClones(payload);
  assert.equal(validated.clone_regions.length, 1);
  const region = validated.clone_regions[0];
  assert.equal(region.max_similarity, null);
  assert.equal(region.partners[0].symbol, null);
  assert.equal(region.refine.refactorability, 1);
  assert.equal(validated.clone_graph.generation, 3);

  // Drift (missing/null numerics, wrong shapes) must coerce instead of crashing providers.
  const drift = validateFileClones({ clone_regions: [{}], clone_graph: null });
  assert.equal(drift.clone_regions.length, 1);
  assert.equal(drift.clone_regions[0].max_similarity, null);
  assert.deepEqual(drift.clone_regions[0].partners, []);
  assert.equal(drift.clone_regions[0].refine, undefined);
  assert.equal(drift.clone_graph, undefined);
});

test('client authenticates JSON and SSE requests', async (t) => {
  const requests = [];
  const server = http.createServer((request, response) => {
    requests.push({ url: request.url, authorization: request.headers.authorization });
    if (request.url === '/api/health') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end('{"status":"ok"}');
      return;
    }
    if (request.url === '/api/events') {
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      response.write('event: version\r');
      setTimeout(
        () =>
          response.end(
            '\ndata: {"generation":7,"max_indexed_at_ms":11,"git_dirty":null,"revision":"r1"}\r\n\r\n',
          ),
        5,
      );
      return;
    }
    response.writeHead(404).end();
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => server.close());

  const LensClient = await loadClient();
  const address = server.address();
  const resolver = {
    invalidate() {},
    async resolve() {
      return { baseUrl: `http://127.0.0.1:${address.port}`, token: 'secret-token' };
    },
  };
  const client = new LensClient(resolver);
  assert.deepEqual(await client.health(), { status: 'ok' });

  let version;
  await assert.rejects(
    client.watchVersions(new AbortController().signal, (event) => {
      version = event;
    }),
    /disconnected/,
  );
  assert.equal(version.revision, 'r1');
  assert.deepEqual(
    requests.map((request) => request.authorization),
    ['Bearer secret-token', 'Bearer secret-token'],
  );
});

test('linked abort signals fire from either side without AbortSignal.any', async () => {
  const { eitherSignal } = await loadClientModule();

  // VS Code 1.85's desktop host is Node 18, where `AbortSignal.any` does not exist.
  const deadline = new AbortController();
  const caller = new AbortController();
  const linked = eitherSignal(deadline.signal, caller.signal);
  assert.equal(linked.aborted, false);
  caller.abort(new Error('caller went away'));
  assert.equal(linked.aborted, true);
  assert.match(String(linked.reason), /caller went away/);

  // An already-aborted input aborts the composition immediately, and no caller means no wrapper.
  const late = new AbortController();
  late.abort(new Error('already gone'));
  assert.equal(eitherSignal(late.signal, new AbortController().signal).aborted, true);
  assert.equal(eitherSignal(late.signal), late.signal);
});

test('stalled SSE reads abort and cancel before reconnecting', async () => {
  const { readWithTimeout } = await loadClientModule();
  const cleanup = [];
  const reader = {
    read: () => new Promise(() => {}),
    cancel: async () => {
      cleanup.push('cancel');
    },
  };

  await assert.rejects(
    readWithTimeout(reader, () => cleanup.push('abort'), 5),
    /produced no data/,
  );
  assert.deepEqual(cleanup, ['abort', 'cancel']);
});

test('equivalent hosted URL spellings share a token key', async () => {
  const { normalizeServerUrl, obsoleteServerTokenSecret, serverTokenSecret } = await loadSourceModule(
    'discovery.ts',
    {},
  );
  const withSlash = 'https://lens.example/';
  const normalized = 'https://lens.example';

  assert.equal(serverTokenSecret(withSlash), serverTokenSecret(normalized));
  assert.equal(obsoleteServerTokenSecret(withSlash, normalized), undefined);
  assert.equal(
    obsoleteServerTokenSecret('https://old.example', normalized),
    serverTokenSecret('https://old.example'),
  );
  assert.throws(() => normalizeServerUrl('https://lens.example/rag-rat'), /must not include a path/);
  assert.throws(() => normalizeServerUrl('https://lens.example/?repo=a'), /must not include a path/);
  // `origin` would drop these silently, so the endpoint would not carry the credentials the user
  // entered — the same rule the server applies to its own origin allowlist.
  assert.throws(
    () => normalizeServerUrl('https://user:pass@lens.example'),
    /must not embed credentials/,
  );
  assert.throws(() => normalizeServerUrl('https://user@lens.example'), /must not embed credentials/);
});

test('automatic discovery accepts the full IPv4 loopback range only', async () => {
  const { isLoopbackHost } = await loadSourceModule('discovery.ts', {});

  assert.equal(isLoopbackHost('127.0.0.1'), true);
  assert.equal(isLoopbackHost('127.0.0.2'), true);
  assert.equal(isLoopbackHost('127.255.255.255'), true);
  assert.equal(isLoopbackHost('localhost'), true);
  assert.equal(isLoopbackHost('[::1]'), true);
  assert.equal(isLoopbackHost('126.255.255.255'), false);
  assert.equal(isLoopbackHost('127.999.0.1'), false);
  assert.equal(isLoopbackHost('127.example'), false);
});

test('clone minimum token setting accepts integers only', () => {
  const packageJson = require('../package.json');
  const setting = packageJson.contributes.configuration.properties['rag-rat-lens.cloneMinTokens'];

  assert.equal(setting.type, 'integer');
});

/** The output channel the extension opens at import; the sidebar logs failures to it. */
const fakeOutput = () => ({
  createOutputChannel: () => ({ appendLine() {}, show() {} }),
});

test('clone sidebar distinguishes unavailable analysis from an empty result', async () => {
  const vscode = {
    EventEmitter: class {},
    ThemeColor: class {},
    ThemeIcon: class {},
    TreeItem: class {},
    TreeItemCollapsibleState: { Expanded: 1, None: 0 },
    window: fakeOutput(),
  };
  const { cloneGraphUnavailableReason } = await loadSourceModule('sidebar.ts', vscode);

  assert.equal(
    cloneGraphUnavailableReason(null),
    'clone graph is unavailable; rebuild the index',
  );
  assert.equal(
    cloneGraphUnavailableReason({ eligible: false, unavailable_reason: 'normalizer_mismatch' }),
    'clone graph is incompatible; rebuild the index',
  );
  assert.equal(cloneGraphUnavailableReason({ eligible: true }), undefined);
});

test('papertrail sidebar keeps unknown states separate from closed items', async () => {
  const vscode = {
    EventEmitter: class {},
    ThemeColor: class {},
    ThemeIcon: class {},
    TreeItem: class {},
    TreeItemCollapsibleState: { Expanded: 1, None: 0 },
    window: fakeOutput(),
  };
  const { groupPapertrailRefs } = await loadSourceModule('sidebar.ts', vscode);
  const refs = ['open', 'closed', 'merged', null, 'draft'].map((state_normalized) => ({
    state_normalized,
  }));

  const grouped = groupPapertrailRefs(refs);
  assert.deepEqual(grouped.open.map((ref) => ref.state_normalized), ['open']);
  assert.deepEqual(grouped.closed.map((ref) => ref.state_normalized), ['closed', 'merged']);
  assert.deepEqual(grouped.unknown.map((ref) => ref.state_normalized), [null, 'draft']);
});

test('subdirectory roots map from either supported workspace opening', async () => {
  const { indexedPath, indexedRootPrefixForDiscovery, workspacePath } = await loadSourceModule(
    'workspace_paths.ts',
  );

  const fromWorktreeTop = indexedRootPrefixForDiscovery('crate', '');
  assert.equal(fromWorktreeTop, 'crate');
  assert.equal(indexedPath('crate/src/lib.rs', fromWorktreeTop), 'src/lib.rs');
  assert.equal(workspacePath('src/lib.rs', fromWorktreeTop), 'crate/src/lib.rs');
  assert.equal(indexedPath('README.md', fromWorktreeTop), undefined);

  const fromIndexedRoot = indexedRootPrefixForDiscovery('crate', 'crate');
  assert.equal(fromIndexedRoot, '');
  assert.equal(indexedPath('src/lib.rs', fromIndexedRoot), 'src/lib.rs');
  assert.equal(workspacePath('src/lib.rs', fromIndexedRoot), 'src/lib.rs');
  assert.equal(indexedRootPrefixForDiscovery('crate', 'crate/src'), undefined);
  assert.equal(indexedRootPrefixForDiscovery('Crate', 'crate', true), '');
  assert.equal(indexedRootPrefixForDiscovery('Crate', 'crate', false), undefined);
  assert.equal(indexedPath('Crate/src/lib.rs', 'crate', true), 'src/lib.rs');
});

test('case-insensitive prefixes fold like the filesystem, not like lowercasing', async () => {
  const { indexedPath, indexedRootPrefixForDiscovery } = await loadSourceModule(
    'workspace_paths.ts',
  );

  // Final vs medial sigma, and decomposed vs composed 'ä': one name to a case-insensitive volume,
  // two distinct strings to `toLowerCase`. Dropping them here would never reach the server's own
  // canonical-path lookup.
  assert.equal(indexedPath('ς/src/lib.rs', 'σ', true), 'src/lib.rs');
  assert.equal(indexedPath('a\u0308/src/lib.rs', '\u00e4', true), 'src/lib.rs');
  assert.equal(indexedRootPrefixForDiscovery('σ', 'ς', true), '');
  // Dotless `ı` uppercases to `I` in JavaScript but folds to itself on the server; accepting it
  // would strip a prefix the server rejects and resolve the rest against another directory.
  assert.equal(indexedPath('ı/src/lib.rs', 'I', true), undefined);
  assert.equal(indexedPath('ı/src/lib.rs', 'i', true), undefined);
  assert.equal(indexedPath('I/src/lib.rs', 'i', true), 'src/lib.rs');
  // Case sensitivity is still honoured, and a different name is still a different name.
  assert.equal(indexedPath('ς/src/lib.rs', 'σ', false), undefined);
  assert.equal(indexedPath('τ/src/lib.rs', 'σ', true), undefined);
  // `ß` folds to two characters; the remainder must still be cut at the segment boundary.
  assert.equal(indexedPath('ß/src/lib.rs', 'SS', true), 'src/lib.rs');
});

test('a literal backslash names a file, it does not separate directories', async () => {
  const { indexedPath, indexedRootPrefixForDiscovery, workspacePath, workspaceRelativePath } =
    await loadSourceModule('workspace_paths.ts');

  // `src/foo\bar.rs` and `src/foo/bar.rs` are two different documents on a Unix checkout. Folding
  // the first onto the second shows one file's signals over the other when both are indexed, and
  // silences every lane when only the backslash-named one is.
  assert.equal(indexedPath('crate/foo\\bar.rs', 'crate'), 'foo\\bar.rs');
  assert.equal(indexedPath('crate/foo/bar.rs', 'crate'), 'foo/bar.rs');
  assert.equal(workspacePath('foo\\bar.rs', 'crate'), 'crate/foo\\bar.rs');
  // A backslash inside a segment belongs to that segment's name, so it cannot match the prefix.
  assert.equal(indexedPath('crate\\foo/bar.rs', 'crate'), undefined);
  assert.equal(indexedRootPrefixForDiscovery('crate', 'crate\\sub'), undefined);
  assert.equal(indexedRootPrefixForDiscovery('crate\\sub', 'crate'), undefined);

  // Absolute paths in either host's spelling are refused, never rewritten into relative ones.
  for (const absolute of [
    '/repo/src/lib.rs',
    '\\\\server\\share\\lib.rs',
    'C:/repo/src/lib.rs',
    'C:\\repo\\src\\lib.rs',
  ]) {
    assert.equal(indexedPath(absolute, ''), undefined, absolute);
    assert.equal(workspacePath(absolute, ''), undefined, absolute);
  }
  // Traversal stays refused under EITHER separator. Refusing costs at most one file's signals,
  // while accepting a segment a Windows host would read as `..` resolves it outside the workspace.
  assert.equal(indexedPath('crate/..\\lib.rs', 'crate'), undefined);
  assert.equal(indexedPath('crate/sub\\..\\lib.rs', 'crate'), undefined);
  assert.equal(workspacePath('..\\lib.rs', 'crate'), undefined);

  // The workspace-folder strip happens in URI space for the same reason.
  assert.equal(workspaceRelativePath('/repo', '/repo/src/foo\\bar.rs'), 'src/foo\\bar.rs');
  assert.equal(workspaceRelativePath('/repo/', '/repo/src/lib.rs'), 'src/lib.rs');
  assert.equal(workspaceRelativePath('/repo', '/repo'), undefined);
  assert.equal(workspaceRelativePath('/repo', '/other/src/lib.rs'), undefined);
  // A Windows client's `file:` URI path is `/c:/repo` — still `/`-separated, still matches.
  assert.equal(workspaceRelativePath('/c:/repo', '/c:/repo/src/lib.rs'), 'src/lib.rs');
  // Both operands are client URIs and `getWorkspaceFolder` has already matched them, which on a
  // case-insensitive client it does whatever the prefix's casing is. The strip folds so a document
  // opened through a differently-cased path keeps its signals; nothing here selects a folder.
  assert.equal(workspaceRelativePath('/c:/repo', '/c:/Repo/src/lib.rs'), 'src/lib.rs');
  assert.equal(workspaceRelativePath('/c:/Repo', '/c:/repo/src/lib.rs'), 'src/lib.rs');
  // Folding the prefix is not folding the remainder: what is stripped is the folder, and the rest
  // of the path is returned verbatim for the indexed-root comparison to judge.
  assert.equal(workspaceRelativePath('/c:/Repo', '/c:/repo/Src/lib.rs'), 'Src/lib.rs');
});

test('a leading drive letter is only absolute when a separator follows it', async () => {
  const { indexedPath, workspacePath } = await loadSourceModule('workspace_paths.ts');

  // `R:notes.md` is a legal file name on a Unix checkout and the server renders it verbatim, so
  // reading a bare letter-colon as a Windows drive would silence a file that exists — the same
  // "treat a legal POSIX name as Windows syntax" mistake as folding `\` into a separator.
  assert.equal(indexedPath('R:notes.md', ''), 'R:notes.md');
  assert.equal(workspacePath('R:notes.md', ''), 'R:notes.md');
  assert.equal(indexedPath('crate/R:notes.md', 'crate'), 'R:notes.md');
  assert.equal(workspacePath('R:notes.md', 'crate'), 'crate/R:notes.md');
  assert.equal(indexedPath('a:b/lib.rs', ''), 'a:b/lib.rs');
});

test('the typed indexed root is the one value validated for host spellings', async () => {
  const { normalizeIndexedRootOverride } = await loadSourceModule('workspace_paths.ts');

  // Every other path in the pipeline is `/`-separated by construction — `vscode.Uri.path` on any
  // host, and the server's own rendering of a repository path. A typed one is not: a Windows user
  // spells the boundary `\`, and storing `crate\sub` would leave a prefix that matches no document
  // at all, silencing every lane for the whole workspace with no error anywhere.
  assert.equal(normalizeIndexedRootOverride('crate/sub'), 'crate/sub');
  assert.equal(normalizeIndexedRootOverride(''), '');
  assert.equal(normalizeIndexedRootOverride('.'), '');
  for (const rejected of [
    'crate\\sub',
    'crate\\',
    '\\crate',
    'C:sub',
    'C:/repo/crate',
    'C:\\repo\\crate',
    '/repo/crate',
    '../elsewhere',
    'crate//sub',
  ]) {
    assert.equal(normalizeIndexedRootOverride(rejected), undefined, rejected);
  }
});

test('indexed-root workspace discovers the worktree file and maps navigation', async () => {
  const fakeUri = (uriPath) => ({
    scheme: 'file',
    path: uriPath,
    toString: () => `file://${uriPath}`,
    with: ({ path: replacement }) => fakeUri(replacement),
  });
  const folder = { uri: fakeUri('/repo/crate') };
  const reads = [];
  const vscode = {
    Uri: {
      joinPath: (base, ...segments) => fakeUri(path.posix.resolve(base.path, ...segments)),
    },
    workspace: {
      workspaceFolders: [folder],
      isTrusted: true,
      fs: {
        readFile: async (uri) => {
          reads.push(uri.path);
          if (uri.path !== '/repo/.rag-rat/sockets/lens.json') {
            throw new Error('not found');
          }
          return new TextEncoder().encode(
            JSON.stringify({
              schema: 'rag-rat-lens-discovery',
              version: 1,
              url: 'http://127.0.0.1:18120',
              indexed_root: 'crate',
              case_insensitive_paths: false,
              ownership_token: 'owner',
            }),
          );
        },
        stat: async (uri) => {
          if (uri.path === '/repo/.git') {
            return { type: 1 };
          }
          throw new Error('not found');
        },
      },
      getWorkspaceFolder: () => folder,
      // Relative paths come from `Uri.path`, never from `asRelativePath`, which answers with the
      // host's separators and so cannot distinguish a directory boundary from a `\` in a name.
      asRelativePath: () => {
        throw new Error('asRelativePath must not be consulted for indexed paths');
      },
    },
  };
  const { LensEndpointResolver } = await loadSourceModule('discovery.ts', vscode);
  const resolver = new LensEndpointResolver(
    { inspect: () => ({ globalValue: '' }) },
    { get: async () => undefined },
  );

  assert.deepEqual(await resolver.resolve(), {
    baseUrl: 'http://127.0.0.1:18120',
    token: 'owner',
  });
  assert.deepEqual(reads.slice(0, 2), [
    '/repo/crate/.rag-rat/sockets/lens.json',
    '/repo/.rag-rat/sockets/lens.json',
  ]);
  assert.equal(resolver.pathOf({ uri: fakeUri('/repo/crate/src/lib.rs') }), 'src/lib.rs');
  assert.equal(resolver.uriOf('src/lib.rs').path, '/repo/crate/src/lib.rs');
});

/**
 * A discovery-backed resolver whose workspace folder sits at the indexed root, parameterised by the
 * folder's URI path so the same assertions run for a POSIX host (`/repo`) and a Windows one
 * (`/c:/repo`, which is what `Uri.file('c:\\repo').path` produces).
 */
async function loopbackResolver(folderPath, discoveryOverrides = {}) {
  const fakeUri = (uriPath, scheme = 'file') => ({
    scheme,
    path: uriPath,
    authority: '',
    toString: () => `${scheme}://${uriPath}`,
    with: ({ path: replacement }) => fakeUri(replacement, scheme),
  });
  const folder = { uri: fakeUri(folderPath) };
  const vscode = {
    Uri: {
      joinPath: (base, ...segments) => fakeUri(path.posix.resolve(base.path, ...segments)),
    },
    workspace: {
      workspaceFolders: [folder],
      isTrusted: true,
      fs: {
        readFile: async (uri) => {
          if (uri.path !== `${folderPath}/.rag-rat/sockets/lens.json`) {
            throw new Error('not found');
          }
          return new TextEncoder().encode(
            JSON.stringify({
              schema: 'rag-rat-lens-discovery',
              version: 1,
              url: 'http://127.0.0.1:18120',
              indexed_root: '',
              case_insensitive_paths: false,
              ownership_token: 'owner',
              ...discoveryOverrides,
            }),
          );
        },
        stat: async (uri) => {
          if (uri.path === `${folderPath}/.git`) {
            return { type: 1 };
          }
          throw new Error('not found');
        },
      },
      getWorkspaceFolder: (uri) => (uri.path.startsWith(`${folderPath}/`) ? folder : undefined),
      asRelativePath: () => {
        throw new Error('asRelativePath must not be consulted for indexed paths');
      },
    },
  };
  const { LensEndpointResolver } = await loadSourceModule('discovery.ts', vscode);
  const resolver = new LensEndpointResolver(
    { inspect: () => ({ globalValue: '' }) },
    { get: async () => undefined },
  );
  await resolver.resolve();
  return { resolver, fakeUri };
}

test('document paths stay in URI space on both a POSIX and a Windows host', async () => {
  for (const folderPath of ['/repo', '/c:/repo']) {
    const { resolver, fakeUri } = await loopbackResolver(folderPath);

    // The two names are distinct documents and must map to distinct indexed paths, in both
    // directions. Rewriting `\` as a separator collapses them onto one.
    assert.equal(
      resolver.pathOf({ uri: fakeUri(`${folderPath}/src/foo\\bar.rs`) }),
      'src/foo\\bar.rs',
      folderPath,
    );
    assert.equal(
      resolver.pathOf({ uri: fakeUri(`${folderPath}/src/foo/bar.rs`) }),
      'src/foo/bar.rs',
      folderPath,
    );
    assert.equal(resolver.uriOf('src/foo\\bar.rs').path, `${folderPath}/src/foo\\bar.rs`);
    assert.equal(resolver.uriOf('src/foo/bar.rs').path, `${folderPath}/src/foo/bar.rs`);
    assert.equal(resolver.uriOf('src/lib.rs').path, `${folderPath}/src/lib.rs`);

    // Unsaved buffers, documents outside the folder, and a document whose URI belongs to another
    // scheme all still answer nothing.
    assert.equal(
      resolver.pathOf({ uri: fakeUri(`${folderPath}/src/lib.rs`), isDirty: true }),
      undefined,
    );
    assert.equal(resolver.pathOf({ uri: fakeUri('/elsewhere/src/lib.rs') }), undefined);
    assert.equal(
      resolver.pathOf({ uri: fakeUri(`${folderPath}/src/lib.rs`, 'vscode-vfs') }),
      undefined,
    );

    if (folderPath === '/repo') {
      // Windows forbids `:` in a file name, so this one can only exist on a Unix checkout — where
      // it is ordinary, and where reading its leading `R:` as a drive would map it to nothing.
      assert.equal(resolver.pathOf({ uri: fakeUri('/repo/R:notes.md') }), 'R:notes.md');
      assert.equal(resolver.uriOf('R:notes.md').path, '/repo/R:notes.md');
    }
  }
});

test('an embedded loopback server still governs case folding for client paths', async () => {
  // Loopback discovery reads a file on this machine and refuses a non-loopback URL, so the
  // server's filesystem IS the client's and its flag describes both.
  const folded = await loopbackResolver('/repo', {
    indexed_root: 'Crate',
    case_insensitive_paths: true,
  });
  assert.equal(
    folded.resolver.pathOf({ uri: folded.fakeUri('/repo/crate/src/lib.rs') }),
    'src/lib.rs',
  );

  const exact = await loopbackResolver('/repo', {
    indexed_root: 'Crate',
    case_insensitive_paths: false,
  });
  assert.equal(exact.resolver.pathOf({ uri: exact.fakeUri('/repo/crate/src/lib.rs') }), undefined);
  assert.equal(
    exact.resolver.pathOf({ uri: exact.fakeUri('/repo/Crate/src/lib.rs') }),
    'src/lib.rs',
  );
});

test('hosted indexed-root overrides stay scoped to one workspace association', async (t) => {
  const fakeUri = (uriPath) => ({
    scheme: 'vscode-vfs',
    path: uriPath,
    authority: '',
    toString: () => `vscode-vfs://${uriPath}`,
    with: ({ path: replacement }) => fakeUri(replacement),
  });
  const atIndexedRoot = { uri: fakeUri('/repo-a') };
  const atWorktreeTop = { uri: fakeUri('/repo-b') };
  let folder = atIndexedRoot;
  const vscode = {
    workspace: {
      get workspaceFolders() {
        return [folder];
      },
      getWorkspaceFolder: () => folder,
      asRelativePath: () => {
        throw new Error('asRelativePath must not be consulted for indexed paths');
      },
    },
  };
  const previousFetch = global.fetch;
  t.after(() => {
    global.fetch = previousFetch;
  });
  const requests = [];
  global.fetch = async (url, options) => {
    requests.push({ url: String(url), authorization: options.headers.Authorization });
    return {
      ok: true,
      json: async () => ({
        repo_id: 'repo-a',
        worktree_id: '',
        indexed_root: 'crate',
        case_insensitive_paths: true,
      }),
    };
  };
  const {
    associateHostedWorkspace,
    hostedRepoAssociationSecret,
    LensEndpointResolver,
    serverTokenSecret,
  } = await loadSourceModule('discovery.ts', vscode);
  const baseUrl = 'https://lens.example';
  const config = {
    inspect: (key) => ({ globalValue: key === 'serverUrl' ? `${baseUrl}/` : undefined }),
  };
  const stored = new Map([[serverTokenSecret(baseUrl), 'hosted-token']]);
  const secrets = {
    get: async (key) => stored.get(key),
    store: async (key, value) => {
      stored.set(key, value);
    },
  };

  // One workspace is opened at the indexed subdirectory and needs the override; the other is
  // opened at its worktree top and must keep following the server's own metadata.
  await associateHostedWorkspace(baseUrl, 'hosted-token', secrets, '.');
  folder = atWorktreeTop;
  await associateHostedWorkspace(baseUrl, 'hosted-token', secrets);
  assert.deepEqual(
    JSON.parse(stored.get(hostedRepoAssociationSecret(baseUrl, atIndexedRoot.uri.toString()))),
    { repoId: 'repo-a', worktreeId: '', indexedRoot: '.' },
  );

  const fromTop = new LensEndpointResolver(config, secrets);
  assert.deepEqual(await fromTop.resolve(), { baseUrl, token: 'hosted-token' });
  assert.equal(fromTop.pathOf({ uri: fakeUri('/repo-b/crate/src/lib.rs') }), 'src/lib.rs');
  // `/api/status` reports `case_insensitive_paths: true`, but that describes the SERVER's
  // filesystem. A hosted server can be another machine, so a differently-cased local directory
  // must NOT be accepted as the indexed root — that would strip the prefix and resolve the
  // remainder against a different directory, painting its signals over the wrong files.
  assert.equal(fromTop.pathOf({ uri: fakeUri('/repo-b/Crate/src/lib.rs') }), undefined);
  assert.equal(fromTop.pathOf({ uri: fakeUri('/repo-b/src/lib.rs') }), undefined);
  // That rule governs the indexed-root prefix ONLY. The workspace-folder strip above it compares
  // two client URIs whose containment VS Code has already decided — `getWorkspaceFolder` returned
  // this folder for this document, which on a case-insensitive client it does for a URI built
  // outside the explorer (a terminal link, a compiler diagnostic) carrying different casing.
  // Applying the hosted rule there would empty every lane for that document with no error and
  // nothing to set: the indexed-root override cannot correct a folder-prefix mismatch.
  assert.equal(fromTop.pathOf({ uri: fakeUri('/Repo-B/crate/src/lib.rs') }), 'src/lib.rs');
  assert.equal(fromTop.pathOf({ uri: fakeUri('/Repo-B/Crate/src/lib.rs') }), undefined);

  folder = atIndexedRoot;
  const fromIndexedRoot = new LensEndpointResolver(config, secrets);
  assert.deepEqual(await fromIndexedRoot.resolve(), { baseUrl, token: 'hosted-token' });
  assert.equal(fromIndexedRoot.pathOf({ uri: fakeUri('/repo-a/src/lib.rs') }), 'src/lib.rs');

  await assert.rejects(
    associateHostedWorkspace(baseUrl, 'hosted-token', secrets, '../elsewhere'),
    /safe workspace-relative path/,
  );
  // An override spelled the host's way is refused where it is typed. Storing one would leave a
  // prefix no document can match: the mapping compares against `/`-separated URI paths, so
  // `crate\sub` is a single segment named `crate\sub` and every lane goes quiet, permanently and
  // silently, for the whole workspace.
  for (const hostSpelled of ['crate\\sub', 'crate\\', 'C:sub', 'C:\\repo\\crate']) {
    await assert.rejects(
      associateHostedWorkspace(baseUrl, 'hosted-token', secrets, hostSpelled),
      /safe workspace-relative path/,
      hostSpelled,
    );
  }
  // A stored override that predates that check fails loudly at resolve time instead of mapping
  // nothing, so the user is told to re-run "rag-rat Lens: Configure Server".
  stored.set(
    hostedRepoAssociationSecret(baseUrl, atIndexedRoot.uri.toString()),
    JSON.stringify({ repoId: 'repo-a', worktreeId: '', indexedRoot: 'crate\\sub' }),
  );
  await assert.rejects(
    new LensEndpointResolver(config, secrets).resolve(),
    /safe workspace-relative path/,
  );
  stored.set(
    hostedRepoAssociationSecret(baseUrl, atIndexedRoot.uri.toString()),
    JSON.stringify({ repoId: 'repo-b', worktreeId: '' }),
  );
  await assert.rejects(
    new LensEndpointResolver(config, secrets).resolve(),
    /no longer serves the checkout/,
  );
  // Linked worktrees of one repository share a `repo_id`, so the checkout half has to be checked
  // on its own: this server answers for the bound repository from a different working tree.
  stored.set(
    hostedRepoAssociationSecret(baseUrl, atIndexedRoot.uri.toString()),
    JSON.stringify({ repoId: 'repo-a', worktreeId: '/repo-a/.worktrees/branch' }),
  );
  await assert.rejects(
    new LensEndpointResolver(config, secrets).resolve(),
    /no longer serves the checkout/,
  );
  // A record predating the checkout binding cannot be checked, so it fails closed as absent.
  stored.set(
    hostedRepoAssociationSecret(baseUrl, atIndexedRoot.uri.toString()),
    JSON.stringify({ repoId: 'repo-a' }),
  );
  await assert.rejects(
    new LensEndpointResolver(config, secrets).resolve(),
    /not associated with this workspace/,
  );
  stored.set(hostedRepoAssociationSecret(baseUrl, atIndexedRoot.uri.toString()), 'repo-a');
  await assert.rejects(
    new LensEndpointResolver(config, secrets).resolve(),
    /not associated with this workspace/,
  );
  assert.ok(requests.length > 0);
  assert.deepEqual(
    requests.filter(
      (request) =>
        request.url !== `${baseUrl}/api/status` || request.authorization !== 'Bearer hosted-token',
    ),
    [],
  );
});

test('diagnostics drop for documents an invalidation does not refresh', async () => {
  const entries = new Map();
  const vscode = {
    languages: {
      createDiagnosticCollection: () => ({
        set: (uri, diagnostics) => entries.set(uri.toString(), diagnostics),
        delete: (uri) => entries.delete(uri.toString()),
        forEach: (visit) => {
          for (const [key, diagnostics] of [...entries]) {
            visit({ toString: () => key }, diagnostics);
          }
        },
        clear: () => entries.clear(),
        dispose: () => {},
      }),
    },
    Range: class {},
    Diagnostic: class {
      constructor(range, message, severity) {
        Object.assign(this, { range, message, severity });
      }
    },
    DiagnosticSeverity: { Warning: 1, Information: 2 },
  };
  const { LensDiagnostics } = await loadSourceModule('diagnostics.ts', vscode);
  const document = (uriPath) => ({
    uri: { toString: () => uriPath },
    lineCount: 10,
    lineAt: () => ({ text: 'code' }),
  });
  const diverged = [{ title: 'Memory', line: 2, verdict: 'diverged', verdict_direction: null }];

  const diagnostics = new LensDiagnostics();
  const visible = document('/repo/src/visible.rs');
  const hidden = document('/repo/src/hidden.rs');
  diagnostics.apply(visible, diverged);
  diagnostics.apply(hidden, diverged);
  assert.deepEqual([...entries.keys()], ['/repo/src/visible.rs', '/repo/src/hidden.rs']);

  // Only visible editors are re-fetched on an index change; a hidden document's entries would
  // otherwise keep asserting a verdict from the previous index state.
  diagnostics.retainOnly([visible.uri]);
  assert.deepEqual([...entries.keys()], ['/repo/src/visible.rs']);

  diagnostics.clear();
  assert.deepEqual([...entries.keys()], []);
});

test('memory documents notify VS Code when cached content changes', async () => {
  const changed = [];
  const vscode = {
    EventEmitter: class {
      event = () => {};
      fire(uri) {
        changed.push(uri.path);
      }
      dispose() {}
    },
    Uri: {
      parse: (raw) => ({ path: new URL(raw).pathname }),
    },
  };
  const { MemoryDocProvider } = await loadSourceModule('memories.ts', vscode);
  let memory = {
    id: 'mem-1',
    title: 'Title',
    kind: 'Invariant',
    confidence: 'high',
    anchor_status: 'current',
    binding_kind: 'path',
    body: 'Before',
    summary: null,
    verdict: null,
  };
  const provider = new MemoryDocProvider({
    data: async (path) => path === 'src/lib.rs' ? { memories: [memory] } : undefined,
  });

  const uri = provider.update(memory, 'src/lib.rs');
  assert.match(provider.provideTextDocumentContent(uri), /Before/);
  memory = { ...memory, body: 'After' };
  await provider.refresh();
  assert.match(provider.provideTextDocumentContent(uri), /After/);
  assert.deepEqual(changed, ['/mem-1.md', '/mem-1.md']);
});

test('clone overlay ranges include the final reported line', async () => {
  const vscode = {
    Range: class {
      constructor(startLine, startCharacter, endLine, endCharacter) {
        Object.assign(this, { startLine, startCharacter, endLine, endCharacter });
      }
    },
  };
  const { inclusiveLineRange } = await loadSourceModule('overlays.ts', vscode);
  const lines = ['one', 'two', 'three'];
  const range = inclusiveLineRange(
    { lineCount: lines.length, lineAt: (line) => ({ text: lines[line] }) },
    1,
    3,
  );

  assert.deepEqual(
    { startLine: range.startLine, startCharacter: range.startCharacter, endLine: range.endLine, endCharacter: range.endCharacter },
    { startLine: 0, startCharacter: 0, endLine: 2, endCharacter: 5 },
  );
});

test('clone overlays reuse decoration types for classes with the same rendered style', async () => {
  const created = [];
  const vscode = {
    MarkdownString: class {
      appendMarkdown() {}
    },
    OverviewRulerLane: { Left: 1 },
    Range: class {
      constructor(startLine, startCharacter, endLine, endCharacter) {
        Object.assign(this, { startLine, startCharacter, endLine, endCharacter });
      }
    },
    window: {
      visibleTextEditors: [],
      createTextEditorDecorationType: (options) => {
        created.push(options);
        return { dispose() {} };
      },
    },
  };
  const { LensOverlays } = await loadSourceModule('overlays.ts', vscode);
  const overlays = new LensOverlays();
  const editor = {
    document: {
      lineCount: 1,
      lineAt: () => ({ text: 'one' }),
      positionAt: () => ({ line: 0 }),
    },
    setDecorations() {},
  };
  const region = (class_id) => ({
    start_line: 1,
    end_line: 1,
    byte_offset: 0,
    class_id,
    max_similarity: 0.96,
    partners: [],
  });

  overlays.toggle();
  overlays.apply(editor, [region(1)], 'src/lib.rs');
  overlays.apply(editor, [region(11)], 'src/lib.rs');

  assert.equal(created.length, 1);
});

test('unknown memory verdict directions remain unknown', async () => {
  const { verdictDirectionLabel } = await loadSourceModule('verdict.ts');

  assert.equal(verdictDirectionLabel('code_ahead'), 'the code moved ahead of this note');
  assert.equal(verdictDirectionLabel('note_ahead'), 'the note moved ahead of the code');
  assert.equal(verdictDirectionLabel('unknown'), 'direction unknown');
  assert.equal(verdictDirectionLabel(null), 'direction unknown');
});

test('a slow clone lane does not discard the lanes that answered', async () => {
  const FileStore = await loadStore();
  const client = {
    fileSymbolGraph: async () => [{ name: 'target' }],
    fileClonesFull: async () => {
      throw new Error('/api/file/clones timed out');
    },
    fileMemories: async () => [{ title: 'Invariant', line: 3 }],
    fileCoupling: async () => [{ path: 'src/other.rs' }],
    filePapertrail: async () => ({ refs: [{ item_key: '5' }], decisions: [] }),
  };
  const store = new FileStore(client, () => 0.9, () => 100, () => 'src/lib.rs', endpoint);
  store.setOnline(true);

  const data = await store.data('src/lib.rs');
  assert.equal(data.memories.length, 1, 'a clone timeout must not discard memories');
  assert.equal(data.symbols.length, 1);
  assert.equal(data.coupling.length, 1);
  assert.equal(data.refs.length, 1);
  // With nothing to fall back on, the failed lane reads as unavailable — not as "no clones here".
  assert.deepEqual(data.clones, []);
  assert.equal(data.cloneGraph.eligible, false);
  assert.ok(data.cloneGraph.unavailable_reason);
  // Worth logging, but the file's signals stay on screen.
  assert.ok(store.failure('src/lib.rs'));
  assert.equal(store.shouldClearSignals('src/lib.rs'), false);
});

test('a failed lane keeps its last value instead of reporting an empty one', async () => {
  const FileStore = await loadStore();
  let memoriesFail = false;
  const client = {
    fileSymbolGraph: async () => [],
    fileClonesFull: async () => ({ clone_regions: [], clone_graph: { eligible: true } }),
    fileMemories: async () => {
      if (memoriesFail) {
        throw new Error('/api/file/memories failed');
      }
      return [{ title: 'Diverged memory', line: 3, verdict: 'diverged' }];
    },
    fileCoupling: async () => [],
    filePapertrail: async () => ({ refs: [], decisions: [] }),
  };
  let answering = 'http://127.0.0.1:18120 owner-token';
  const store = new FileStore(
    client,
    () => 0.9,
    () => 100,
    () => 'src/lib.rs',
    async () => answering,
  );
  store.setOnline(true);
  assert.equal((await store.data('src/lib.rs')).memories.length, 1);

  // The index moved and the memory lane dropped its request: reporting zero memories would clear
  // a real diverged-memory warning over one failed request.
  memoriesFail = true;
  store.invalidate();
  const afterFailure = await store.data('src/lib.rs');
  assert.equal(afterFailure.memories.length, 1, 'the last known memories survive a failed lane');
  assert.equal(afterFailure.memories[0].verdict, 'diverged');

  // Discovery can re-point silently — a restarted server mints a fresh token — and no caller
  // announces that. The fallback is keyed by who answered, so it drops itself rather than mixing
  // one repository's memories into another's identically named file.
  answering = 'http://127.0.0.1:18121 other-token';
  store.invalidate();
  assert.deepEqual((await store.data('src/lib.rs')).memories, []);
});

test('a load that spans two servers is reloaded, not merged', async () => {
  const FileStore = await loadStore();
  let served = 'first';
  const client = {
    fileSymbolGraph: async () => [{ name: served }],
    fileClonesFull: async () => ({ clone_regions: [], clone_graph: { eligible: true } }),
    fileMemories: async () => [{ title: 'Memory', line: 1 }],
    fileCoupling: async () => [],
    filePapertrail: async () => ({ refs: [], decisions: [] }),
  };
  // The client retries a failed lane against the replacement endpoint, so discovery re-pointing
  // mid-load can leave one repository's lanes beside another's. Identity is read before and after
  // the lanes: this one changes across the first attempt and is stable across the retry.
  const answering = ['18120 first', '18121 second'];
  let asked = 0;
  const store = new FileStore(
    client,
    () => 0.9,
    () => 100,
    () => 'src/lib.rs',
    async () => {
      const identity = answering[Math.min(asked++, answering.length - 1)];
      served = identity;
      return identity;
    },
  );
  store.setOnline(true);

  const data = await store.data('src/lib.rs');
  assert.equal(
    data.symbols[0].name,
    '18121 second',
    'the retry is served wholly by the endpoint that settled, never a mixture',
  );
  assert.equal(
    store.shouldClearSignals('src/lib.rs'),
    false,
    're-pointing is not a failure — signals are replaced, not cleared',
  );
});

test('an unconfirmable endpoint drops the payload without failing the file', async (t) => {
  const FileStore = await loadStore();
  const client = {
    fileSymbolGraph: async () => [{ name: 'target' }],
    fileClonesFull: async () => ({ clone_regions: [], clone_graph: { eligible: true } }),
    fileMemories: async () => [{ title: 'Memory', line: 1 }],
    fileCoupling: async () => [],
    filePapertrail: async () => ({ refs: [], decisions: [] }),
  };
  // Identity is read again after the lanes, and that read can fail on its own — the discovery file
  // is momentarily absent while `rag-rat mcp` restarts, which is the very event this survives.
  let asked = 0;
  const store = new FileStore(client, () => 0.9, () => 100, () => 'src/lib.rs', async () => {
    asked += 1;
    if (asked === 2) {
      throw new Error('no Lens discovery file found');
    }
    return 'one server';
  });
  store.setOnline(true);

  assert.equal(await store.data('src/lib.rs'), undefined, 'an unconfirmed payload is not served');
  assert.equal(
    store.shouldClearSignals('src/lib.rs'),
    false,
    'five successful lanes must not be turned into a file-level failure that wipes the editor',
  );
  assert.equal(store.failure('src/lib.rs'), undefined, 'nothing failed — the answer was unusable');
});

test('an invalidation during the identity read is not overwritten', async () => {
  const FileStore = await loadStore();
  let loads = 0;
  const client = {
    fileSymbolGraph: async () => {
      loads += 1;
      return [{ name: `load-${loads}` }];
    },
    fileClonesFull: async () => ({ clone_regions: [], clone_graph: { eligible: true } }),
    fileMemories: async () => [],
    fileCoupling: async () => [],
    filePapertrail: async () => ({ refs: [], decisions: [] }),
  };
  const gate = deferred();
  let asked = 0;
  const store = new FileStore(client, () => 0.9, () => 100, () => 'src/lib.rs', async () => {
    asked += 1;
    if (asked === 2) {
      await gate.promise;
    }
    return 'one server';
  });
  store.setOnline(true);

  const request = store.data('src/lib.rs');
  await new Promise((resolve) => setTimeout(resolve, 0));
  // The index moved while this load was confirming its endpoint. Committing it now would put back
  // what the invalidation cleared, and the TTL would then serve it for another ten seconds.
  store.invalidate();
  gate.resolve();
  assert.equal(await request, undefined, 'an overtaken load is not committed');

  const reloaded = await store.data('src/lib.rs');
  assert.equal(reloaded.symbols[0].name, 'load-2', 'the next read refetches instead of the stale one');
});

test('the served-source counter moves on a re-point and a reset, not on a reindex', async () => {
  const FileStore = await loadStore();
  const client = {
    fileSymbolGraph: async () => [],
    fileClonesFull: async () => ({ clone_regions: [], clone_graph: { eligible: true } }),
    fileMemories: async () => [],
    fileCoupling: async () => [],
    filePapertrail: async () => ({ refs: [], decisions: [] }),
  };
  const server = { identity: 'http://127.0.0.1:18120 owner-token' };
  const store = new FileStore(client, () => 0.9, () => 100, () => 'src/lib.rs', async () => server.identity);
  store.setOnline(true);
  await store.data('src/lib.rs');
  const served = store.sourceEpoch();

  // The index moved under the same server. Anything already drawn from it is out of date, and its
  // replacement is on the way — not wrong, so a consumer has no reason to take it down.
  store.invalidate();
  await store.data('src/lib.rs');
  assert.equal(store.sourceEpoch(), served, 'a reindex is not a change of source');

  // Discovery re-points with nothing having declared it: `rag-rat mcp` restarted on another port,
  // or minted a fresh ownership token. A consumer holding rows from the previous server has no
  // other way to learn that they describe a different index.
  server.identity = 'http://127.0.0.1:18121 restarted-token';
  store.invalidate();
  await store.data('src/lib.rs');
  const repointed = store.sourceEpoch();
  assert.notEqual(repointed, served, 'another server is another source');

  store.invalidate();
  await store.data('src/lib.rs');
  assert.equal(store.sourceEpoch(), repointed, 'the same server twice is one source');

  // The explicit declaration: a reconfigured server or a replaced workspace folder.
  store.reset();
  assert.notEqual(store.sourceEpoch(), repointed, 'a reset makes everything served unusable');
});

test('a lane that keeps failing stops being carried forward', async (t) => {
  const FileStore = await loadStore();
  const realNow = Date.now;
  t.after(() => {
    Date.now = realNow;
  });
  let clock = realNow();
  Date.now = () => clock;
  let memoriesFail = false;
  const client = {
    fileSymbolGraph: async () => [],
    fileClonesFull: async () => ({ clone_regions: [], clone_graph: { eligible: true } }),
    fileMemories: async () => {
      if (memoriesFail) {
        throw new Error('/api/file/memories failed');
      }
      return [{ title: 'Memory', line: 3 }];
    },
    fileCoupling: async () => [],
    filePapertrail: async () => ({ refs: [], decisions: [] }),
  };
  const store = new FileStore(client, () => 0.9, () => 100, () => 'src/lib.rs', endpoint);
  store.setOnline(true);
  assert.equal((await store.data('src/lib.rs')).memories.length, 1);

  // Each merged payload becomes the next one's fallback, so without an age bound a lane that keeps
  // failing would re-assert its line-anchored warning forever — over a file being edited under it.
  memoriesFail = true;
  clock += 30_000;
  store.invalidate();
  assert.equal((await store.data('src/lib.rs')).memories.length, 1, 'still recent enough to carry');

  clock += 40_000;
  store.invalidate();
  assert.deepEqual(
    (await store.data('src/lib.rs')).memories,
    [],
    'past the bound the honest answer is nothing, not a minute-old claim',
  );
});

test('an invalidation aborts the requests whose answers it discarded', async () => {
  const FileStore = await loadStore();
  const signals = [];
  const hang = () => new Promise(() => {});
  const record = (path, signal) => {
    signals.push(signal);
    return hang();
  };
  const client = {
    fileSymbolGraph: record,
    fileClonesFull: (path, theta, minTokens, signal) => record(path, signal),
    fileMemories: record,
    fileCoupling: record,
    filePapertrail: record,
  };
  const store = new FileStore(client, () => 0.9, () => 100, () => 'src/lib.rs', endpoint);
  store.setOnline(true);

  void store.data('src/lib.rs');
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(signals.length, 5, 'every lane carries the load signal');
  assert.ok(signals.every((signal) => signal && !signal.aborted));

  // The clone lane can be a repository-wide scan; an index change that discards its answer should
  // stop it rather than let the server run it to completion for a reader that has gone away.
  store.invalidate();
  assert.ok(signals.every((signal) => signal.aborted), 'discarded requests are aborted');
});

test('tracked files are bounded so a long session cannot grow without limit', async () => {
  const FileStore = await loadStore();
  let memoriesFail = false;
  const client = {
    fileSymbolGraph: async () => [],
    fileClonesFull: async () => ({ clone_regions: [], clone_graph: { eligible: true } }),
    fileMemories: async () => {
      if (memoriesFail) {
        throw new Error('/api/file/memories failed');
      }
      return [{ title: 'Memory', line: 1 }];
    },
    fileCoupling: async () => [],
    filePapertrail: async () => ({ refs: [], decisions: [] }),
  };
  const store = new FileStore(client, () => 0.9, () => 100, () => 'src/lib.rs', endpoint);
  store.setOnline(true);
  assert.equal((await store.data('src/first.rs')).memories.length, 1);

  // Visiting far more files than the cap evicts the oldest, fallback included — otherwise every
  // file a session ever touched would keep its payload alive for the lifetime of the window.
  for (let i = 0; i < 300; i++) {
    await store.data(`src/f${i}.rs`);
  }
  memoriesFail = true;
  store.invalidate();
  assert.deepEqual(
    (await store.data('src/first.rs')).memories,
    [],
    'the evicted file has no fallback left to serve',
  );
});

test('a file whose every lane fails clears its signals', async () => {
  const FileStore = await loadStore();
  const failing = async () => {
    throw new Error('connection refused');
  };
  const client = {
    fileSymbolGraph: failing,
    fileClonesFull: failing,
    fileMemories: failing,
    fileCoupling: failing,
    filePapertrail: failing,
  };
  const store = new FileStore(client, () => 0.9, () => 100, () => 'src/lib.rs', endpoint);
  store.setOnline(true);

  assert.equal(await store.data('src/lib.rs'), undefined);
  assert.equal(store.shouldClearSignals('src/lib.rs'), true);
  assert.ok(store.failure('src/lib.rs'));
});

test('file store stays empty while the server is unreachable and drops in-flight data', async () => {
  const FileStore = await loadStore();
  const pending = Array.from({ length: 5 }, deferred);
  let loads = 0;
  const client = {
    fileSymbolGraph: async () => {
      loads += 1;
      return pending[0].promise;
    },
    fileClonesFull: async () => pending[1].promise,
    fileMemories: async () => pending[2].promise,
    fileCoupling: async () => pending[3].promise,
    filePapertrail: async () => pending[4].promise,
  };
  const store = new FileStore(client, () => 0.9, () => 100, () => 'src/lib.rs', endpoint);

  assert.equal(store.shouldClearSignals('src/lib.rs'), true);
  assert.equal(await store.data('src/lib.rs'), undefined);
  assert.equal(loads, 0, 'offline providers must not reach the HTTP client');

  store.setOnline(true);
  assert.equal(
    store.shouldClearSignals('src/lib.rs'),
    false,
    'an online epoch race must not clear current signals',
  );
  const request = store.data('src/lib.rs');
  store.setOnline(false);
  assert.equal(
    store.shouldClearSignals('src/lib.rs'),
    true,
    'a hidden editor selected while offline must clear stale signals',
  );
  pending[0].resolve([]);
  pending[1].resolve({ clone_regions: [], clone_graph: null });
  pending[2].resolve([]);
  pending[3].resolve([]);
  pending[4].resolve({ refs: [], decisions: [] });
  assert.equal(await request, undefined, 'a disconnect invalidates an in-flight response');
  assert.equal(await store.data('src/lib.rs'), undefined);
  assert.equal(loads, 1, 'offline reads must not repopulate stale data');
});

test('an unsaved buffer has no indexed path, so every line-anchored surface goes quiet', async () => {
  const fakeUri = (uriPath) => ({
    scheme: 'file',
    path: uriPath,
    toString: () => `file://${uriPath}`,
  });
  const folder = { uri: fakeUri('/repo') };
  const vscode = {
    Uri: {
      joinPath: (base, ...segments) => fakeUri(path.posix.resolve(base.path, ...segments)),
    },
    workspace: {
      workspaceFolders: [folder],
      getWorkspaceFolder: () => folder,
      asRelativePath: (uri) => path.posix.relative(folder.uri.path, uri.path),
    },
  };
  const { LensEndpointResolver } = await loadSourceModule('discovery.ts', vscode);
  const resolver = new LensEndpointResolver(
    { inspect: () => ({ globalValue: '' }) },
    { get: async () => undefined },
  );

  const uri = fakeUri('/repo/src/lib.rs');
  assert.equal(resolver.pathOf({ uri, isDirty: false }), 'src/lib.rs');
  // Every fact the server can offer is anchored to line numbers in the SAVED file, so one
  // inserted line puts all of them on the wrong statement.
  assert.equal(
    resolver.pathOf({ uri, isDirty: true }),
    undefined,
    'a dirty buffer must not be mapped to the indexed file',
  );
});

test('a replaced workspace folder cannot reuse the previous folder\'s server mapping', async () => {
  const fakeUri = (uriPath) => ({
    scheme: 'file',
    path: uriPath,
    toString: () => `file://${uriPath}`,
  });
  const discovery = (port) =>
    new TextEncoder().encode(
      JSON.stringify({
        schema: 'rag-rat-lens-discovery',
        version: 1,
        url: `http://127.0.0.1:${port}`,
        indexed_root: '',
        case_insensitive_paths: false,
        ownership_token: `owner-${port}`,
      }),
    );
  const folders = { current: { uri: fakeUri('/first') } };
  const vscode = {
    Uri: {
      joinPath: (base, ...segments) => fakeUri(path.posix.resolve(base.path, ...segments)),
    },
    workspace: {
      get workspaceFolders() {
        return [folders.current];
      },
      isTrusted: true,
      fs: {
        readFile: async (uri) => {
          if (uri.path === '/first/.rag-rat/sockets/lens.json') {
            return discovery(18120);
          }
          if (uri.path === '/second/.rag-rat/sockets/lens.json') {
            return discovery(18121);
          }
          throw new Error('not found');
        },
        stat: async (uri) => {
          if (uri.path === '/first/.git' || uri.path === '/second/.git') {
            return { type: 1 };
          }
          throw new Error('not found');
        },
      },
      getWorkspaceFolder: () => folders.current,
      asRelativePath: (uri) => path.posix.relative(folders.current.uri.path, uri.path),
    },
  };
  const { LensEndpointResolver } = await loadSourceModule('discovery.ts', vscode);
  const resolver = new LensEndpointResolver(
    { inspect: () => ({ globalValue: '' }) },
    { get: async () => undefined },
  );

  assert.deepEqual(await resolver.resolve(), {
    baseUrl: 'http://127.0.0.1:18120',
    token: 'owner-18120',
  });

  // A single-root window can replace its folder without reloading the extension host. Well inside
  // the discovery TTL, so a cache keyed only on time (or on the server URL) would answer with the
  // previous repository's endpoint and path mapping.
  folders.current = { uri: fakeUri('/second') };
  assert.deepEqual(
    await resolver.resolve(),
    { baseUrl: 'http://127.0.0.1:18121', token: 'owner-18121' },
    'a resolution describes a workspace, not just a server',
  );
});

test('a resolution that finishes after its workspace was replaced must not publish', async () => {
  const fakeUri = (uriPath) => ({
    scheme: 'file',
    path: uriPath,
    toString: () => `file://${uriPath}`,
  });
  const folders = { current: { uri: fakeUri('/first') } };
  const gate = deferred();
  const vscode = {
    Uri: {
      joinPath: (base, ...segments) => fakeUri(path.posix.resolve(base.path, ...segments)),
    },
    workspace: {
      get workspaceFolders() {
        return [folders.current];
      },
      isTrusted: true,
      fs: {
        readFile: async (uri) => {
          if (uri.path !== '/first/.rag-rat/sockets/lens.json') {
            throw new Error('not found');
          }
          // Hold the resolution open so the folder can be replaced mid-flight.
          await gate.promise;
          return new TextEncoder().encode(
            JSON.stringify({
              schema: 'rag-rat-lens-discovery',
              version: 1,
              url: 'http://127.0.0.1:18120',
              indexed_root: 'crate',
              case_insensitive_paths: false,
              ownership_token: 'owner-first',
            }),
          );
        },
        stat: async (uri) => {
          if (uri.path === '/first/.git') {
            return { type: 1 };
          }
          throw new Error('not found');
        },
      },
      getWorkspaceFolder: () => folders.current,
      asRelativePath: (uri) => path.posix.relative(folders.current.uri.path, uri.path),
    },
  };
  const { LensEndpointResolver } = await loadSourceModule('discovery.ts', vscode);
  const resolver = new LensEndpointResolver(
    { inspect: () => ({ globalValue: '' }) },
    { get: async () => undefined },
  );

  const inFlight = resolver.resolve();
  folders.current = { uri: fakeUri('/second') };
  gate.resolve();

  await assert.rejects(
    inFlight,
    /superseded/,
    'a resolution describing the previous folder must not install its path mapping',
  );
  // The stale `indexed_root: 'crate'` prefix must not have landed: with it installed, a file at
  // the new folder's root would be reported as outside the indexed tree.
  assert.equal(resolver.pathOf({ uri: fakeUri('/second/src/lib.rs'), isDirty: false }), 'src/lib.rs');
});

test('data that arrives after its document went dirty is withheld, not drawn', async () => {
  const FileStore = await loadStore();
  const pending = [];
  const client = {
    fileSymbolGraph: () => new Promise((resolve) => pending.push(resolve)),
    fileClonesFull: () => new Promise((resolve) => pending.push(resolve)),
    fileMemories: () => new Promise((resolve) => pending.push(resolve)),
    fileCoupling: () => new Promise((resolve) => pending.push(resolve)),
    filePapertrail: () => new Promise((resolve) => pending.push(resolve)),
  };
  const document = { uri: { toString: () => 'file:///repo/src/lib.rs' }, isDirty: false };
  const store = new FileStore(
    client,
    () => 0.9,
    () => 100,
    // The real resolver withholds a path for a dirty buffer; mirror that here.
    (doc) => (doc.isDirty ? undefined : 'src/lib.rs'),
    endpoint,
  );
  store.setOnline(true);

  const request = store.dataFor(document);
  // Let the store resolve its endpoint and dispatch all five lanes before disturbing anything.
  for (let tick = 0; tick < 50 && pending.length < 5; tick += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
  assert.equal(pending.length, 5, 'all five lanes must be in flight');
  // The user types while they are.
  document.isDirty = true;
  pending[0]({ symbols: [] });
  pending[1]({ clone_regions: [], clone_graph: null });
  pending[2]({ memories: [] });
  pending[3]({ coupling: [] });
  pending[4]({ refs: [], decisions: [] });

  assert.equal(
    await request,
    undefined,
    'an answer for the saved file must not be drawn over the edited buffer',
  );
  // Saving restores it: the same cached payload is now legitimate again.
  document.isDirty = false;
  const loaded = await store.dataFor(document);
  assert.equal(loaded.path, 'src/lib.rs');
});

/** Everything the three sidebar views render, tagged so each one's source file is identifiable. */
function sidebarData(name) {
  return {
    at: 0,
    symbols: [],
    coupling: [],
    clones: [
      { start_line: 1, end_line: 2, class_id: 1, max_similarity: 0.9, symbol: `clone_of_${name}`, partners: [] },
    ],
    cloneGraph: { eligible: true },
    memories: [{ title: `memory_of_${name}`, kind: 'Invariant', line: 1 }],
    refs: [
      {
        item_key: '1',
        title: `ref_of_${name}`,
        item_kind: 'issue',
        ref_kind: 'mention',
        source_text: '',
        state_normalized: 'open',
        url: null,
      },
    ],
    decisions: [],
  };
}

const sidebarEditor = (name) => ({
  document: { uri: { toString: () => `file:///repo/src/${name}` } },
});

/** Let every queued microtask and the pump's own continuation run. */
async function settle() {
  for (let tick = 0; tick < 5; tick += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
}

/** The three views wired to a fake window, with the store's async surface under test control. */
async function sidebarHarness(store) {
  const active = { editor: undefined };
  const listeners = [];
  const providers = new Map();
  const fires = [];
  const logged = [];
  const vscode = {
    EventEmitter: class {
      fire(value) {
        fires.push(value);
      }
      get event() {
        return () => ({ dispose() {} });
      }
    },
    ThemeColor: class {},
    ThemeIcon: class {
      constructor(id) {
        this.id = id;
      }
    },
    TreeItem: class {
      constructor(label) {
        this.label = label;
      }
    },
    TreeItemCollapsibleState: { Expanded: 1, None: 0 },
    window: {
      get activeTextEditor() {
        return active.editor;
      },
      registerTreeDataProvider: (id, provider) => {
        providers.set(id, provider);
        return { dispose() {} };
      },
      onDidChangeActiveTextEditor: (listener) => {
        listeners.push(listener);
        return { dispose() {} };
      },
      createOutputChannel: () => ({
        appendLine: (line) => logged.push(line),
        show() {},
      }),
    },
  };
  const { registerSidebar } = await loadSourceModule('sidebar.ts', vscode);
  const sidebar = registerSidebar({ subscriptions: [] }, store);
  return {
    refresh: () => sidebar.refresh(),
    /** Make `editor` active, as the host does before it delivers the change event. */
    focus(editor) {
      active.editor = editor;
    },
    activate(editor) {
      active.editor = editor;
      for (const listener of listeners) {
        listener(editor);
      }
    },
    /** What one view is showing right now — a synchronous read, as VS Code performs it. */
    shown(view = 'cloneClasses') {
      const items = providers.get(`rag-rat-lens.${view}`).getChildren();
      assert.ok(Array.isArray(items), 'getChildren must answer synchronously');
      return JSON.stringify(items);
    },
    /** How many times the views have told VS Code to re-read — one per view per publication. */
    published: () => fires.length,
    /** What reached the extension's output channel. */
    logged: () => logged.join('\n'),
  };
}

test('a sidebar load that settles after a newer one cannot overwrite it', async () => {
  const gates = { 'first.rs': deferred(), 'second.rs': deferred() };
  const asked = [];
  const harness = await sidebarHarness({
    dataEpoch: () => 0,
    sourceEpoch: () => 0,
    pathOf: () => 'src/whatever.rs',
    async dataFor(document) {
      const name = document.uri.toString().split('/').pop();
      asked.push(name);
      await gates[name].promise;
      return { path: `src/${name}`, data: sidebarData(name) };
    },
  });

  harness.activate(sidebarEditor('first.rs'));
  // Both loads are now in flight: the user switched while the first was still running.
  harness.activate(sidebarEditor('second.rs'));
  assert.deepEqual(asked, ['first.rs', 'second.rs']);
  // Neither load has answered, so no file's tree is up — the views are still on the row they were
  // created with. (What happens to a tree that IS up when the document changes is pinned by "a
  // document switch takes the previous file down before the new one loads".)
  assert.match(harness.shown(), /loading…/);
  assert.doesNotMatch(harness.shown(), /from_first|clone_of_first/);

  gates['second.rs'].resolve();
  await settle();
  assert.match(harness.shown(), /clone_of_second\.rs/);
  assert.match(harness.shown('memories'), /memory_of_second\.rs/);
  assert.match(harness.shown('papertrail'), /ref_of_second\.rs/);
  const published = harness.published();

  // The OLDER load settles last. `getChildren` has no way to decline to publish, so under the
  // previous shape whichever invocation settled last decided what the sidebar showed; here the
  // load itself declines, and publishes nothing at all.
  gates['first.rs'].resolve();
  await settle();
  assert.match(harness.shown(), /clone_of_second\.rs/);
  assert.doesNotMatch(harness.shown(), /clone_of_first\.rs/);
  assert.equal(harness.published(), published, 'a superseded load must not publish anything');
});

test('a sidebar load settling between the editor moving and the event arriving is dropped', async () => {
  const gate = deferred();
  const harness = await sidebarHarness({
    dataEpoch: () => 0,
    sourceEpoch: () => 0,
    pathOf: () => 'src/first.rs',
    async dataFor(document) {
      const name = document.uri.toString().split('/').pop();
      await gate.promise;
      return { path: `src/${name}`, data: sidebarData(name) };
    },
  });

  harness.activate(sidebarEditor('first.rs'));
  const published = harness.published();
  // `activeTextEditor` is updated by the host before the change event reaches an extension, so a
  // load can settle inside that window — with nothing having told the pump to reload yet, which
  // is the one thing a generation counter cannot see.
  harness.focus(sidebarEditor('second.rs'));
  gate.resolve();
  await settle();
  assert.doesNotMatch(harness.shown(), /clone_of_first/, 'not a tree for a file that moved off');
  assert.equal(harness.published(), published);

  // The event arrives and the sidebar fills for the file the user is actually on.
  harness.activate(sidebarEditor('second.rs'));
  await settle();
  assert.match(harness.shown(), /clone_of_second/);
});

test('a same-document sidebar load that settles last cannot put the older payload back', async () => {
  const gates = [deferred(), deferred()];
  let started = 0;
  const harness = await sidebarHarness({
    dataEpoch: () => 0,
    sourceEpoch: () => 0,
    pathOf: () => 'src/lib.rs',
    async dataFor() {
      const load = started++;
      await gates[load].promise;
      return { path: 'src/lib.rs', data: sidebarData(`load_${load}`) };
    },
  });

  // Two loads for the SAME document under the SAME index state, and they can still disagree: the
  // store re-points and reloads wholly against a replacement endpoint without moving its epoch, so
  // the earlier request can be carrying an older server's answer. Neither the document nor the
  // epoch separates them — only which request is the newest.
  harness.activate(sidebarEditor('lib.rs'));
  harness.refresh();
  gates[1].resolve();
  await settle();
  assert.match(harness.shown(), /clone_of_load_1/);
  const published = harness.published();

  gates[0].resolve();
  await settle();
  assert.match(harness.shown(), /clone_of_load_1/);
  assert.doesNotMatch(harness.shown(), /clone_of_load_0/, 'the overtaken load must not publish');
  assert.equal(harness.published(), published);
});

test('an invalidation while a sidebar load runs neither blanks the tree nor republishes', async () => {
  const epoch = { value: 0 };
  const gate = { current: deferred() };
  const answer = { name: 'index_1' };
  const harness = await sidebarHarness({
    dataEpoch: () => epoch.value,
    sourceEpoch: () => 0,
    pathOf: () => 'src/lib.rs',
    async dataFor() {
      const started = epoch.value;
      await gate.current.promise;
      // What the real store does: a payload computed under a superseded epoch is not returned,
      // and neither is one for a server that did not answer. The caller sees the same `undefined`.
      return started === epoch.value && answer.name
        ? { path: 'src/lib.rs', data: sidebarData(answer.name) }
        : undefined;
    },
  });

  harness.activate(sidebarEditor('lib.rs'));
  gate.current.resolve();
  await settle();
  assert.match(harness.shown(), /clone_of_index_1/);
  const published = harness.published();

  // The index moves while the next load is in flight. Its answer is withheld by the store, and
  // rendering that withholding as an outage would clear a tree over an ordinary reindex.
  gate.current = deferred();
  harness.refresh();
  epoch.value += 1;
  gate.current.resolve();
  await settle();
  assert.match(harness.shown(), /clone_of_index_1/, 'an overtaken load must not blank the tree');
  assert.doesNotMatch(harness.shown(), /offline/);
  assert.equal(harness.published(), published, 'an overtaken load must not publish anything');

  // The refresh that every invalidation is paired with is what repaints, from the new index state.
  answer.name = 'index_2';
  gate.current = deferred();
  harness.refresh();
  gate.current.resolve();
  await settle();
  assert.match(harness.shown(), /clone_of_index_2/);

  // Withholding is for a superseded premise only: a server that stops answering under a stable
  // index state still has to be reported, or the sidebar would show the last good tree forever.
  answer.name = '';
  gate.current = deferred();
  harness.refresh();
  gate.current.resolve();
  await settle();
  assert.match(harness.shown(), /lens server offline/);
});

test('the sidebar shows a loading row before anything has asked it to load', async () => {
  const harness = await sidebarHarness({
    dataEpoch: () => 0,
    sourceEpoch: () => 0,
    pathOf: () => 'src/lib.rs',
    dataFor: async () => undefined,
  });

  // The views exist from `registerSidebar` and are filled by the first pump. An empty tree in that
  // window is a claim about the file — that it has no clones, no memories, no tracker items —
  // which nothing has established.
  assert.match(harness.shown(), /loading…/);
  assert.match(harness.shown('memories'), /loading…/);
  assert.match(harness.shown('papertrail'), /loading…/);
});

test('a document switch takes the previous file down before the new one loads', async () => {
  const gate = { current: deferred() };
  const harness = await sidebarHarness({
    dataEpoch: () => 0,
    sourceEpoch: () => 0,
    pathOf: () => 'src/lib.rs',
    async dataFor(document) {
      const name = document.uri.toString().split('/').pop();
      await gate.current.promise;
      return { path: `src/${name}`, data: sidebarData(name) };
    },
  });

  harness.activate(sidebarEditor('first.rs'));
  gate.current.resolve();
  await settle();
  assert.match(harness.shown(), /clone_of_first\.rs/);

  // The load for the new document is in flight and the views still hold the old one's tree.
  // Leaving it up for the length of that request states one file's clone classes, memories and
  // tracker items about another.
  gate.current = deferred();
  harness.activate(sidebarEditor('second.rs'));
  await settle();
  assert.doesNotMatch(harness.shown(), /clone_of_first\.rs/, "the previous file's tree must not stand");
  assert.doesNotMatch(harness.shown('memories'), /memory_of_first\.rs/);
  assert.doesNotMatch(harness.shown('papertrail'), /ref_of_first\.rs/);
  assert.match(harness.shown(), /loading…/);

  gate.current.resolve();
  await settle();
  assert.match(harness.shown(), /clone_of_second\.rs/);
});

test('a data-source reset takes the previous tree down before the replacement arrives', async () => {
  const store = { epoch: 0, source: 0 };
  const gate = { current: deferred() };
  const answer = { name: 'server_1' };
  const harness = await sidebarHarness({
    dataEpoch: () => store.epoch,
    sourceEpoch: () => store.source,
    pathOf: () => 'src/lib.rs',
    async dataFor() {
      await gate.current.promise;
      return { path: 'src/lib.rs', data: sidebarData(answer.name) };
    },
  });

  harness.activate(sidebarEditor('lib.rs'));
  gate.current.resolve();
  await settle();
  assert.match(harness.shown(), /clone_of_server_1/);

  // An ordinary index invalidation: same document, same server. What is on screen is merely out of
  // date and its replacement is already loading, so it stays up — clearing it on every version
  // event would flicker all three views for no gain.
  store.epoch += 1;
  gate.current = deferred();
  harness.refresh();
  await settle();
  assert.match(harness.shown(), /clone_of_server_1/, 'a reindex must not blank the tree');
  gate.current.resolve();
  await settle();

  // `store.reset()` — configuring a server, changing Lens configuration, replacing the workspace
  // folder. The active document is unchanged, so nothing about the DOCUMENT says the tree is
  // wrong; what changed is where its rows came from. Every one of them describes a server or a
  // repository that is no longer being asked, and holding them up until the new request answers
  // shows one repository's clones, memories and tracker items over another's file.
  store.epoch += 1;
  store.source += 1;
  answer.name = 'server_2';
  gate.current = deferred();
  harness.refresh();
  await settle();
  assert.doesNotMatch(harness.shown(), /clone_of_server_1/, "another source's tree must not stand");
  assert.doesNotMatch(harness.shown('memories'), /memory_of_server_1/);
  assert.doesNotMatch(harness.shown('papertrail'), /ref_of_server_1/);
  assert.match(harness.shown(), /loading…/);

  gate.current.resolve();
  await settle();
  assert.match(harness.shown(), /clone_of_server_2/);
});

test('a sidebar load that rejects is reported, not left as a spinner', async () => {
  const failing = { now: false };
  const harness = await sidebarHarness({
    dataEpoch: () => 0,
    sourceEpoch: () => 0,
    pathOf: () => 'src/lib.rs',
    async dataFor() {
      if (failing.now) {
        throw new Error('resolving the document path failed');
      }
      return { path: 'src/lib.rs', data: sidebarData('lib') };
    },
  });

  harness.activate(sidebarEditor('lib.rs'));
  await settle();
  assert.match(harness.shown(), /clone_of_lib/);

  // The load path is the only thing that can fill the views, so an error inside it that nothing
  // catches leaves them on whatever the switch put up — a spinner that never resolves, saying the
  // answer is on its way when there is no longer anything coming.
  failing.now = true;
  harness.activate(sidebarEditor('other.rs'));
  await settle();
  assert.doesNotMatch(harness.shown(), /loading…/, 'a failed load must not leave the spinner up');
  assert.doesNotMatch(harness.shown(), /clone_of_lib/);
  assert.match(harness.shown(), /lens data unavailable/);
  assert.match(harness.shown('memories'), /lens data unavailable/);
  assert.match(harness.shown('papertrail'), /lens data unavailable/);
  assert.match(harness.logged(), /resolving the document path failed/);
});

test('a payload one view cannot render leaves all three views agreeing', async () => {
  const answer = { data: sidebarData('good') };
  const harness = await sidebarHarness({
    dataEpoch: () => 0,
    sourceEpoch: () => 0,
    pathOf: () => 'src/lib.rs',
    async dataFor() {
      return { path: 'src/lib.rs', data: answer.data };
    },
  });

  harness.activate(sidebarEditor('lib.rs'));
  await settle();
  assert.match(harness.shown('papertrail'), /ref_of_good/);

  // `/api/file/papertrail` is served to the views unvalidated, so a server version that omits a
  // field the payload is trusted to carry throws in ONE view's builder. Publishing view by view
  // would then leave two views describing that payload and the third on the previous file — the
  // disagreement between panes that the single pump exists to prevent.
  answer.data = { ...sidebarData('broken'), decisions: undefined };
  harness.refresh();
  await settle();
  assert.doesNotMatch(harness.shown(), /clone_of_broken/, 'no view may describe an unrenderable payload');
  assert.match(harness.shown(), /lens data unavailable/);
  assert.match(harness.shown('memories'), /lens data unavailable/);
  assert.match(harness.shown('papertrail'), /lens data unavailable/);
});

test('open memory documents are withdrawn when the server stops answering', async () => {
  const fired = [];
  const vscode = {
    EventEmitter: class {
      fire(value) {
        fired.push(value);
      }
      get event() {
        return () => ({ dispose() {} });
      }
      dispose() {}
    },
    Uri: { parse: (value) => ({ path: value.split(':')[1], toString: () => value }) },
    Range: class {},
    CodeLens: class {},
    ViewColumn: { Beside: 2 },
    window: {},
    workspace: {},
  };
  const { MemoryDocProvider } = await loadSourceModule('memories.ts', vscode);
  const provider = new MemoryDocProvider({});
  const uri = provider.update(
    { id: 'mem-1', title: 'Bound invariant', kind: 'Invariant', body: 'The thing must hold' },
    'src/lib.rs',
  );

  assert.match(provider.provideTextDocumentContent(uri), /Bound invariant/);

  provider.withdraw();
  const withdrawn = provider.provideTextDocumentContent(uri);
  assert.match(withdrawn, /not answering/, 'an outage must be stated, not left showing the memory');
  assert.doesNotMatch(withdrawn, /The thing must hold/, 'the old server\'s claim must be gone');
  // A withdrawal is an outage, NOT a deletion: reporting "memory not found" would claim the index
  // no longer holds it, which is what a real removal renders.
  assert.doesNotMatch(withdrawn, /memory not found/);
  assert.equal(
    provider.provideTextDocumentContent({ path: '/never-existed.md' }),
    'memory not found',
    'a genuinely absent memory still reads as absent',
  );
});

test('a memory refresh in flight when the server drops cannot restore the withdrawn body', async () => {
  const vscode = {
    EventEmitter: class {
      fire() {}
      get event() {
        return () => ({ dispose() {} });
      }
      dispose() {}
    },
    Uri: { parse: (value) => ({ path: value.split(':')[1], toString: () => value }) },
    Range: class {},
    CodeLens: class {},
    ViewColumn: { Beside: 2 },
    window: {},
    workspace: {},
  };
  const { MemoryDocProvider } = await loadSourceModule('memories.ts', vscode);
  const gate = deferred();
  const store = {
    async data() {
      await gate.promise;
      return { memories: [{ id: 'mem-1', title: 'Bound invariant', kind: 'Invariant', body: 'body' }] };
    },
  };
  const provider = new MemoryDocProvider(store);
  const uri = provider.update(
    { id: 'mem-1', title: 'Bound invariant', kind: 'Invariant', body: 'body' },
    'src/lib.rs',
  );

  const refreshing = provider.refresh();
  // The server drops out while that refresh is awaiting its answer.
  provider.withdraw();
  gate.resolve();
  await refreshing;

  assert.match(
    provider.provideTextDocumentContent(uri),
    /not answering/,
    'a pre-withdrawal refresh must not republish the unreachable server\'s memory',
  );
});

test('a command captured before an outage cannot restore a withdrawn memory', async () => {
  const vscode = {
    EventEmitter: class {
      fire() {}
      get event() {
        return () => ({ dispose() {} });
      }
      dispose() {}
    },
    Uri: { parse: (value) => ({ path: value.split(':')[1], toString: () => value }) },
    Range: class {},
    CodeLens: class {},
    ViewColumn: { Beside: 2 },
    window: {},
    workspace: {},
  };
  const { MemoryDocProvider } = await loadSourceModule('memories.ts', vscode);
  const answers = { data: undefined };
  const provider = new MemoryDocProvider({ async data() { return answers.data; } });
  const memory = { id: 'mem-1', title: 'Bound invariant', kind: 'Invariant', body: 'the claim' };
  const uri = provider.update(memory, 'src/lib.rs');
  assert.match(provider.provideTextDocumentContent(uri), /the claim/);

  provider.withdraw();
  // A QuickPick opened before the outage still holds this `FileMemory` and renders it on pick.
  provider.update(memory, 'src/lib.rs');
  assert.match(
    provider.provideTextDocumentContent(uri),
    /not answering/,
    'rendering from a pre-outage capture must not put the old body back',
  );

  // Reachability is what lifts the withdrawal — not a refresh, which iterates nothing when no
  // memory documents are open and would leave the flag stuck forever.
  provider.restore();
  answers.data = { memories: [memory] };
  await provider.refresh();
  assert.match(provider.provideTextDocumentContent(uri), /the claim/);
});

test('a withdrawal made with no documents open does not outlive the outage', async () => {
  const vscode = {
    EventEmitter: class {
      fire() {}
      get event() {
        return () => ({ dispose() {} });
      }
      dispose() {}
    },
    Uri: { parse: (value) => ({ path: value.split(':')[1], toString: () => value }) },
    Range: class {},
    CodeLens: class {},
    ViewColumn: { Beside: 2 },
    window: {},
    workspace: {},
  };
  const { MemoryDocProvider } = await loadSourceModule('memories.ts', vscode);
  const provider = new MemoryDocProvider({ async data() { return { memories: [] }; } });

  // The server drops while nothing is open, so there is no source path for a refresh to walk.
  provider.withdraw();
  await provider.refresh();
  provider.restore();

  const uri = provider.update(
    { id: 'mem-1', title: 'Bound invariant', kind: 'Invariant', body: 'the claim' },
    'src/lib.rs',
  );
  assert.match(
    provider.provideTextDocumentContent(uri),
    /the claim/,
    'a memory opened after the server returned must render, not report an outage',
  );
});

test('a memory picked after the index moved is re-read, never rendered from the captured copy', async () => {
  const vscode = {
    EventEmitter: class {
      fire() {}
      get event() {
        return () => ({ dispose() {} });
      }
      dispose() {}
    },
    Uri: { parse: (value) => ({ path: value.split(':')[1], toString: () => value }) },
    Range: class {},
    CodeLens: class {},
    ViewColumn: { Beside: 2 },
    window: {},
    workspace: {},
  };
  const { MemoryDocProvider } = await loadSourceModule('memories.ts', vscode);
  const answers = { data: undefined };
  const provider = new MemoryDocProvider({ async data() { return answers.data; } });
  const captured = { id: 'mem-1', title: 'Bound invariant', kind: 'Invariant', body: 'OLD body' };

  // The index moved while the picker sat open: same memory, revised body.
  answers.data = { memories: [{ ...captured, body: 'NEW body' }] };
  const ready = await provider.openPicked(captured.id, 'src/lib.rs');
  assert.equal(ready.kind, 'ready');
  const rendered = provider.provideTextDocumentContent(ready.uri);
  assert.match(rendered, /NEW body/);
  assert.doesNotMatch(rendered, /OLD body/, 'the captured copy must not be what gets rendered');

  // The memory was deleted while the picker sat open: rendering the capture would resurrect it.
  answers.data = { memories: [] };
  // "absent", not "deleted": a failed memory lane also produces an empty list, so the payload
  // cannot distinguish the two and neither may the message.
  assert.deepEqual(await provider.openPicked(captured.id, 'src/lib.rs'), { kind: 'absent' });

  // The server stopped answering: an outage is not a deletion.
  answers.data = undefined;
  assert.deepEqual(await provider.openPicked(captured.id, 'src/lib.rs'), { kind: 'unavailable' });
});

test('the memory picker routes through the revalidating open, not the captured object', async () => {
  const shown = [];
  const messages = [];
  const vscode = {
    EventEmitter: class {
      fire() {}
      get event() {
        return () => ({ dispose() {} });
      }
      dispose() {}
    },
    Uri: { parse: (value) => ({ path: value, toString: () => value }) },
    Range: class {},
    CodeLens: class {},
    ViewColumn: { Beside: 2 },
    window: {
      showQuickPick: async (items) => items[0],
      showTextDocument: async (doc) => shown.push(doc),
      showInformationMessage: async (message) => messages.push(message),
    },
    workspace: { openTextDocument: async (uri) => ({ uri }) },
  };
  const { showMemoriesQuickPick } = await loadSourceModule('memories.ts', vscode);
  const captured = { id: 'mem-1', title: 'T', kind: 'Invariant', body: 'OLD', confidence: 'high' };

  const calls = { openPicked: [], update: [] };
  const documents = {
    async openPicked(id, path) {
      calls.openPicked.push([id, path]);
      return { kind: 'ready', uri: 'rag-rat-memory:/mem-1.md' };
    },
    update(memory, path) {
      calls.update.push([memory, path]);
      return 'rag-rat-memory:/mem-1.md';
    },
  };

  await showMemoriesQuickPick([captured], 'src/lib.rs', documents);
  assert.deepEqual(calls.openPicked, [['mem-1', 'src/lib.rs']], 'the pick must be re-read by id');
  assert.deepEqual(calls.update, [], 'the captured object must never be rendered directly');
  assert.equal(shown.length, 1);

  // A memory deleted while the picker was open is reported, not opened.
  shown.length = 0;
  documents.openPicked = async () => ({ kind: 'absent' });
  await showMemoriesQuickPick([captured], 'src/lib.rs', documents);
  assert.equal(shown.length, 0, 'nothing to show for a memory that is gone');
  assert.match(messages.at(-1), /may have been removed, or the lookup for this file failed/);
  assert.doesNotMatch(messages.at(-1), /no longer in the index/, 'deletion must not be asserted');

  // An outage says so, rather than claiming the memory was deleted.
  documents.openPicked = async () => ({ kind: 'unavailable' });
  await showMemoriesQuickPick([captured], 'src/lib.rs', documents);
  assert.match(messages.at(-1), /not answering/);
});

test('open rag-rat-doc documents are withdrawn when the server they came from is gone', async () => {
  const vscode = {
    EventEmitter: class {
      fire() {}
      get event() {
        return () => ({ dispose() {} });
      }
      dispose() {}
    },
    Uri: { parse: (value) => ({ path: value.split(':').slice(1).join(':'), toString: () => value }) },
    Range: class {},
    CodeLens: class {},
    ThemeIcon: class {},
    ViewColumn: { Beside: 2 },
    window: { showTextDocument: async () => undefined },
    workspace: {
      openTextDocument: async (uri) => ({ uri }),
      registerTextDocumentContentProvider: () => ({ dispose() {} }),
    },
    commands: { registerCommand: () => ({ dispose() {} }), executeCommand: async () => undefined },
  };
  const module = await loadSourceModule('lenses.ts', vscode);
  const provider = new module.LensDocProvider();

  // Stand in for a decision record or extraction preview the user opened.
  await provider.open('Extract helper', '# Proposal\n\nlift this into a helper');
  const uri = { path: '/0-Extract%20helper.md' };
  assert.match(provider.provideTextDocumentContent(uri), /lift this into a helper/);

  provider.withdraw();
  const withdrawn = provider.provideTextDocumentContent(uri);
  assert.match(withdrawn, /no longer being served/);
  assert.doesNotMatch(withdrawn, /lift this into a helper/, "the old server's answer must be gone");
  // Withdrawal is not deletion: an unknown key still reads as absent.
  assert.equal(provider.provideTextDocumentContent({ path: '/never.md' }), 'not found');
});

test('caller requests prefer the symbol handle and fall back to the qualified name', async (t) => {
  const requests = [];
  const server = http.createServer((request, response) => {
    requests.push(request.url);
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end('{"callers":[],"resolved_by":"id","matched_symbols":1}');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => server.close());

  const LensClient = await loadClient();
  const address = server.address();
  const client = new LensClient({
    invalidate() {},
    async resolve() {
      return { baseUrl: `http://127.0.0.1:${address.port}`, token: null };
    },
  });

  // Two overloads share `src/lib.rs::run`, so the handle is the only selector that separates
  // them — it must win even when a qualified name is available alongside it.
  await client.symbolCallers({ id: 'sym_2029231588695800bf', qname: 'src/lib.rs::run' });
  const withHandle = new URL(requests[0], 'http://localhost');
  assert.equal(withHandle.searchParams.get('id'), 'sym_2029231588695800bf');
  assert.equal(withHandle.searchParams.get('qname'), null);

  // A row the server could not hand a handle for still navigates, by name.
  await client.symbolCallers({ id: null, qname: 'src/lib.rs::run' });
  const withName = new URL(requests[1], 'http://localhost');
  assert.equal(withName.searchParams.get('qname'), 'src/lib.rs::run');
  assert.equal(withName.searchParams.get('id'), null);

  await assert.rejects(client.symbolCallers({ id: null, qname: null }), /handle or a qualified name/);
  assert.equal(requests.length, 2, 'a selector-less request must never reach the server');

  // The envelope reaches the caller whole: unwrapping to `callers` here would throw away the
  // server's own statement of how many symbols it answered for.
  const answer = await client.symbolCallers({ id: 'sym_2029231588695800bf', qname: null });
  assert.deepEqual(answer, { callers: [], resolved_by: 'id', matched_symbols: 1 });
});

test('the caller quick pick says when its rows are a union rather than one symbol', async () => {
  const quickPicks = [];
  const vscode = {
    EventEmitter: class {
      constructor() {
        this.event = () => ({ dispose() {} });
      }
      fire() {}
      dispose() {}
    },
    window: {
      showQuickPick: async (_items, options) => {
        quickPicks.push(options.placeHolder);
        return undefined;
      },
    },
    workspace: { registerTextDocumentContentProvider: () => ({ dispose() {} }) },
    commands: { registerCommand: () => ({ dispose() {} }), executeCommand: async () => undefined },
  };
  const registered = new Map();
  vscode.commands.registerCommand = (id, handler) => {
    registered.set(id, handler);
    return { dispose() {} };
  };
  const { registerSignalCommands, callersPlaceholder } = await loadSourceModule('lenses.ts', vscode);

  let answer;
  registerSignalCommands({ subscriptions: [] }, { symbolCallers: async () => answer });
  const showCallers = registered.get('rag-rat-lens.showCallers');
  const selector = { id: null, qname: 'src/lib.rs::run' };

  // The qualified name covered both overloads, so these rows belong to two different functions.
  // Reporting them as "callers of run" would state a narrower claim than the answer supports.
  answer = { callers: [{}, {}], resolved_by: 'ref', matched_symbols: 2 };
  await showCallers(selector, 'run');
  assert.equal(quickPicks.at(-1), '2 callers of 2 symbols named run');

  // Resolved by handle: the rows really are one symbol's.
  answer = { callers: [{}], resolved_by: 'id', matched_symbols: 1 };
  await showCallers({ id: 'sym_alpha', qname: 'src/lib.rs::run' }, 'run');
  assert.equal(quickPicks.at(-1), '1 callers of run');

  // Resolved by handle, but the handle covers a GROUP: two overloads that declare identically
  // share one logical symbol, so the server answered for both and said so. The count is the only
  // signal that distinguishes this from the line above, and it is present on this lane too — a
  // reader shown a union has to be told, whichever selector produced it.
  answer = { callers: [{}, {}], resolved_by: 'id', matched_symbols: 2 };
  await showCallers({ id: 'sym_shared', qname: 'src/lib.rs::run' }, 'run');
  assert.equal(quickPicks.at(-1), '2 callers of 2 symbols named run');

  // Nothing indexed carries the name: the rows came from unresolved call sites alone, so calling
  // them that symbol's callers would invent a symbol.
  answer = { callers: [], resolved_by: 'ref', matched_symbols: 0 };
  await showCallers(selector, 'run');
  assert.equal(quickPicks.at(-1), '0 callers by name — nothing indexed is named run');

  // A server too old to report either field says nothing, and neither do we.
  assert.equal(callersPlaceholder('run', 3, {}), '3 callers of run');
});

test('each overload lens carries its own symbol handle', async () => {
  const lenses = [];
  const vscode = {
    CodeLens: class {
      constructor(range, command) {
        Object.assign(this, { range, command });
        lenses.push(this);
      }
    },
    EventEmitter: class {
      constructor() {
        this.event = () => ({ dispose() {} });
      }
      fire() {}
      dispose() {}
    },
    Range: class {
      constructor(startLine) {
        this.startLine = startLine;
      }
    },
  };
  const { SignalLensProvider } = await loadSourceModule('lenses.ts', vscode);

  const overload = (id, startLine) => ({
    id,
    name: 'run',
    qname: 'src/lib.rs::run',
    kind: 'function',
    start_line: startLine,
    end_line: startLine,
    is_test: false,
    callers: { exact: 1, syntactic: 0, name_only: 0, ambiguous: 0, tests: 0, dispatch: 0 },
    fan_in_score: 1,
    fan_in_bucket: 'low',
    dispatch: [],
  });
  const provider = new SignalLensProvider({
    dataFor: async () => ({
      data: {
        symbols: [overload('sym_alpha', 9), overload('sym_beta', 13)],
        coupling: [],
        refs: [],
        decisions: [],
        clones: [],
      },
    }),
  });

  await provider.provideCodeLenses({ lineCount: 40 });
  const selectors = lenses
    .filter((lens) => lens.command.command === 'rag-rat-lens.showCallers')
    .map((lens) => lens.command.arguments[0]);
  assert.deepEqual(selectors, [
    { id: 'sym_alpha', qname: 'src/lib.rs::run' },
    { id: 'sym_beta', qname: 'src/lib.rs::run' },
  ]);
});

test('a lens whose handle covers several declarations says so before it is clicked', async () => {
  const lenses = [];
  const vscode = {
    CodeLens: class {
      constructor(range, command) {
        Object.assign(this, { range, command });
        lenses.push(this);
      }
    },
    EventEmitter: class {
      constructor() {
        this.event = () => ({ dispose() {} });
      }
      fire() {}
      dispose() {}
    },
    Range: class {
      constructor(startLine) {
        this.startLine = startLine;
      }
    },
  };
  const { SignalLensProvider, sharedHandleMarker } = await loadSourceModule('lenses.ts', vscode);

  // The reach is only worth rendering when it exceeds this row: `1` is what almost every symbol
  // reports, and an absent field is a server that cannot say — neither qualifies anything.
  assert.equal(sharedHandleMarker(undefined), undefined);
  assert.equal(sharedHandleMarker(1), undefined);
  assert.equal(sharedHandleMarker(0), undefined);
  assert.equal(sharedHandleMarker(2).title, '  ⧉2');
  assert.match(sharedHandleMarker(2).tooltip, /1 other declaration —/);
  assert.match(sharedHandleMarker(3).tooltip, /2 other declarations —/);

  const symbol = (id_declarations, startLine) => ({
    id: 'sym_shared',
    id_declarations,
    name: 'run',
    qname: 'src/lib.rs::run',
    kind: 'function',
    start_line: startLine,
    end_line: startLine,
    is_test: false,
    callers: { exact: 1, syntactic: 0, name_only: 0, ambiguous: 0, tests: 0, dispatch: 0 },
    fan_in_score: 1,
    fan_in_bucket: 'low',
    dispatch: [],
  });
  const alone = symbol(1, 9);
  const legacy = symbol(undefined, 17);
  delete legacy.id_declarations;
  const provider = new SignalLensProvider({
    dataFor: async () => ({
      data: {
        symbols: [alone, symbol(2, 13), legacy],
        coupling: [],
        refs: [],
        decisions: [],
        clones: [],
      },
    }),
  });

  await provider.provideCodeLenses({ lineCount: 40 });
  const callerLenses = lenses.filter((lens) => lens.command.command === 'rag-rat-lens.showCallers');
  // The count in the title is this declaration's own, but the quick pick behind the grouped row
  // answers for both — so that row, and only that row, carries the qualifier.
  assert.deepEqual(
    callerLenses.map((lens) => lens.command.title),
    ['⤴ 1 (1 exact)', '⤴ 1 (1 exact)  ⧉2', '⤴ 1 (1 exact)'],
  );
  assert.deepEqual(
    callerLenses.map((lens) => lens.command.tooltip),
    [undefined, sharedHandleMarker(2).tooltip, undefined],
  );
});

test('the hover link dispatches show callers exactly as the lens does', async () => {
  const vscode = {
    MarkdownString: class {
      constructor(value) {
        this.value = value ?? '';
      }
      appendMarkdown(text) {
        this.value += text;
        return this;
      }
    },
    Hover: class {
      constructor(contents, range) {
        Object.assign(this, { contents, range });
      }
    },
    Range: class {
      constructor(startLine) {
        this.startLine = startLine;
      }
    },
  };
  const { GraphHoverProvider } = await loadSourceModule('hover.ts', vscode);

  const row = (id, qname, startLine) => ({
    id,
    name: 'run',
    qname,
    kind: 'function',
    start_line: startLine,
    end_line: startLine,
    is_test: false,
    callers: { exact: 1, syntactic: 0, name_only: 0, ambiguous: 0, tests: 0, dispatch: 0 },
    fan_in_score: 1,
    fan_in_bucket: 'low',
    dispatch: [],
  });
  // The last row is what a server built before the handle existed sends: no `id` FIELD at all,
  // rather than a null one.
  const legacy = row(null, 'src/lib.rs::legacy', 25);
  delete legacy.id;
  const grouped = row('sym_shared', 'src/lib.rs::grouped', 29);
  grouped.id_declarations = 2;
  const symbols = [
    row('sym_alpha', 'src/lib.rs::run', 9),
    row('sym_beta', 'src/lib.rs::run', 13),
    row(null, 'src/lib.rs::only_named', 17),
    row(null, null, 21),
    legacy,
    grouped,
  ];
  const provider = new GraphHoverProvider({ dataFor: async () => ({ data: { symbols } }) });
  const hoverText = async (line) => (await provider.provideHover({}, { line: line - 1 })).contents.value;
  const commandArguments = async (line) => {
    const hover = await provider.provideHover({}, { line: line - 1 });
    const link = /command:rag-rat-lens\.showCallers\?([^)]*)\)/.exec(hover.contents.value);
    return link ? JSON.parse(decodeURIComponent(link[1])) : undefined;
  };

  // The hover dispatches the SAME command as the CodeLens, so it has to send what the handler
  // reads — a selector, not a bare name — and its own row's handle: the two overloads share a
  // qualified name, so the name alone answers for both.
  assert.deepEqual(await commandArguments(9), [{ id: 'sym_alpha', qname: 'src/lib.rs::run' }, 'run']);
  assert.deepEqual(await commandArguments(13), [{ id: 'sym_beta', qname: 'src/lib.rs::run' }, 'run']);
  // A row without a handle still navigates by name; a row with neither offers no link at all.
  assert.deepEqual(await commandArguments(17), [
    { id: null, qname: 'src/lib.rs::only_named' },
    'run',
  ]);
  assert.equal(await commandArguments(21), undefined);
  // An absent `id` must be normalized to null before the tuple is serialized: JSON drops an
  // `undefined` field, so the hover would otherwise ship a selector one key short of the one the
  // CodeLens dispatches — a shape difference no typecheck can see across a `command:` URI.
  assert.deepEqual(await commandArguments(25), [{ id: null, qname: 'src/lib.rs::legacy' }, 'run']);
  // The hover offers the same link, so it owes the same qualifier: the counts above it are this
  // declaration's, the link below it answers for both.
  assert.match(await hoverText(29), /⧉ This symbol shares one identity with 1 other declaration/);
  assert.doesNotMatch(await hoverText(9), /⧉/);
});

test('a symbol handle the server no longer knows falls back to the name, not to rediscovery', async (t) => {
  const requests = [];
  const server = http.createServer((request, response) => {
    requests.push(request.url);
    if (new URL(request.url, 'http://localhost').searchParams.has('id')) {
      response.writeHead(404, { 'content-type': 'application/json' });
      response.end('{"error":"unknown symbol handle"}');
      return;
    }
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end('{"callers":[{}],"resolved_by":"ref","matched_symbols":2}');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => server.close());

  const { LensClient, lensHttpStatus } = await loadClientModule();
  const address = server.address();
  let invalidations = 0;
  const client = new LensClient({
    invalidate() {
      invalidations += 1;
    },
    async resolve() {
      return { baseUrl: `http://127.0.0.1:${address.port}`, token: null };
    },
  });

  // The handle is derived from the declaration, so editing a signature mints a new one on the
  // next index pass while the CodeLens already drawn in the gutter still carries the old. The
  // click must still answer — by the name the same row supplied, wider and labelled as such.
  const answer = await client.symbolCallers({ id: 'sym_stale', qname: 'src/lib.rs::run' });
  assert.deepEqual(answer, { callers: [{}], resolved_by: 'ref', matched_symbols: 2 });
  assert.equal(requests.length, 2);
  assert.equal(new URL(requests[0], 'http://localhost').searchParams.get('id'), 'sym_stale');
  assert.equal(
    new URL(requests[1], 'http://localhost').searchParams.get('qname'),
    'src/lib.rs::run',
  );

  // A status line proves the discovered endpoint answered, so discarding it cannot help: it only
  // costs every other reader a rediscovery of the server that just replied.
  assert.equal(invalidations, 0);

  // Nothing to fall back to. The rejection carries the status so the surface can tell a stale
  // lens from a broken server.
  await assert.rejects(
    client.symbolCallers({ id: 'sym_stale', qname: null }),
    (error) => lensHttpStatus(error) === 404,
  );
  assert.equal(invalidations, 0);
});

test('only a failure discovery could fix discards the discovered endpoint', async (t) => {
  let status = 401;
  const server = http.createServer((_request, response) => {
    response.writeHead(status, { 'content-type': 'application/json' });
    response.end('{"error":"nope"}');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => server.close());

  const LensClient = await loadClient();
  const address = server.address();
  let invalidations = 0;
  const client = new LensClient({
    invalidate() {
      invalidations += 1;
    },
    async resolve() {
      return { baseUrl: `http://127.0.0.1:${address.port}`, token: 'rotated' };
    },
  });

  // The credential is what discovery carries, so a rejected one is exactly what re-reading it
  // fixes; so is a server that has stopped behaving like itself.
  await assert.rejects(client.status(), /401/);
  assert.equal(invalidations, 1);
  status = 503;
  await assert.rejects(client.status(), /503/);
  assert.equal(invalidations, 2);

  // A 4xx is a statement about the request. The endpoint understood it and said no; deriving the
  // same endpoint again cannot change that, and costs every other lane its cached identity.
  status = 400;
  await assert.rejects(client.status(), /400/);
  assert.equal(invalidations, 2);
});

test('a click on a lens whose symbol was reindexed says so instead of failing the command', async () => {
  const quickPicks = [];
  const warnings = [];
  const vscode = {
    EventEmitter: class {
      constructor() {
        this.event = () => ({ dispose() {} });
      }
      fire() {}
      dispose() {}
    },
    window: {
      showQuickPick: async (_items, options) => {
        quickPicks.push(options.placeHolder);
        return undefined;
      },
      showWarningMessage: async (message) => {
        warnings.push(message);
        return undefined;
      },
    },
    workspace: { registerTextDocumentContentProvider: () => ({ dispose() {} }) },
    commands: { executeCommand: async () => undefined },
  };
  const registered = new Map();
  vscode.commands.registerCommand = (id, handler) => {
    registered.set(id, handler);
    return { dispose() {} };
  };
  const { registerSignalCommands } = await loadSourceModule('lenses.ts', vscode);

  let failure;
  registerSignalCommands(
    { subscriptions: [] },
    {
      symbolCallers: async () => {
        throw failure;
      },
    },
  );
  const showCallers = registered.get('rag-rat-lens.showCallers');

  // The row carried a handle and no name, so the client had nothing to retry with. Before the
  // handle lane existed this click produced a quick pick; a raw "contributed command failed"
  // notification is not what a stale gutter decoration deserves.
  failure = Object.assign(new Error('/api/symbol/callers -> 404: {"error":"unknown symbol handle"}'), {
    lensHttpStatus: 404,
  });
  await showCallers({ id: 'sym_stale', qname: null }, 'run');
  assert.equal(quickPicks.length, 0);
  assert.match(warnings.at(-1), /run/);
  assert.match(warnings.at(-1), /no longer in the index/);

  // Everything else is still a real failure and must keep surfacing as one.
  failure = Object.assign(new Error('/api/symbol/callers -> 500: boom'), { lensHttpStatus: 500 });
  await assert.rejects(showCallers({ id: 'sym_stale', qname: null }, 'run'), /500/);
  failure = new TypeError('fetch failed');
  await assert.rejects(showCallers({ id: null, qname: 'src/lib.rs::run' }, 'run'), /fetch failed/);
  assert.equal(warnings.length, 1);
});
