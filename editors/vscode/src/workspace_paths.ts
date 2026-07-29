/**
 * Validate a relative path that is already in `/`-separated form, which every value this module
 * sees is: `vscode.Uri.path` is `/`-separated on every host, and the server renders repository
 * paths with `/` on every platform.
 *
 * Separator conversion deliberately does NOT happen here. A `\` is a legal character in a Unix
 * filename, so rewriting it would make `src/foo\bar.rs` name a different document: the request
 * goes out for `src/foo/bar.rs` and whatever the index holds under that name — a genuinely nested
 * file's memories, clones and graph — is drawn over the backslash-named one, with nothing on
 * screen to distinguish it from a correct answer. The platform distinction is made before this
 * point, by the URI layer: a Windows path reaches `Uri.path` as `/c:/repo/src/lib.rs`, and Windows
 * forbids `\` in a filename, so a backslash that survives into a URI path can only be part of a
 * name.
 *
 * The index half of that identity is not fixed yet: `paths::path_string` folds `\` into `/` when a
 * row is written, on every platform, so both names land on one `files.path` (#1032). Until
 * that lands a backslash-named Unix file matches no row and its lanes are empty rather than wrong.
 * That is the direction to be wrong in — silence is visible as "no signals here", a neighbouring
 * file's signals are not — but it does mean this rule alone does not yet make such a file work.
 */
export function normalizeRelativePath(path: string): string | undefined {
  if (!path || path === '.') {
    return '';
  }
  // Absolute in any host's spelling — POSIX root, UNC share, or Windows drive. Refused rather than
  // reinterpreted as a relative path. The drive test requires the separator: `R:notes.md` is an
  // ordinary file name on a Unix checkout and the server reports it verbatim, so reading a bare
  // letter-colon as a drive would silence a file that exists — the same "legal POSIX name treated
  // as Windows syntax" mistake as folding `\` into a separator. The drive-relative `C:sub`
  // spelling reaches this function from no producer; [`normalizeIndexedRootOverride`] refuses it
  // at the one seam that could emit it.
  if (path.startsWith('/') || path.startsWith('\\') || /^[A-Za-z]:[\\/]/.test(path)) {
    return undefined;
  }
  if (path.split('/').some(isUnsafeSegment)) {
    return undefined;
  }
  return path;
}

/**
 * Empty, or reading as `.`/`..` under EITHER host's separator.
 *
 * Traversal is refused backslash-aware even though separators are not rewritten backslash-aware,
 * because the two directions cost differently: refusing a segment costs at most the signals for
 * one oddly named file, while letting `sub\..\lib.rs` through would resolve against a directory
 * outside the workspace once a Windows host joins it onto a filesystem path.
 */
function isUnsafeSegment(segment: string): boolean {
  return !segment || segment.split('\\').some((part) => part === '.' || part === '..');
}

/**
 * Validate the one path in this pipeline a human spells: the indexed-root override entered through
 * "rag-rat Lens: Configure Server".
 *
 * Every other value arrives `/`-separated by construction — `vscode.Uri.path` on any host, and the
 * server's own rendering of a repository path. A typed one does not: a Windows user spells the
 * directory boundary `\` and the drive-relative root `C:sub`, and either would validate as a
 * one-segment relative path, be stored, and then match no document at all — every lens lane silent
 * for the whole workspace, with no error anywhere and no expiry, because the override is persisted.
 *
 * Refusing costs a Unix directory whose name literally contains `\` the ability to be named as an
 * indexed root, which is vanishingly rarer than a Windows user typing the separator their host
 * uses. Making the refusal here is what lets the validators over URI- and server-derived paths stop
 * encoding host syntax that cannot reach them.
 */
export function normalizeIndexedRootOverride(value: string): string | undefined {
  if (value.includes('\\') || /^[A-Za-z]:/.test(value)) {
    return undefined;
  }
  return normalizeRelativePath(value);
}

/** Resolve how the indexed root sits inside the opened workspace. */
export function indexedRootPrefixForDiscovery(
  indexedRoot: string,
  workspacePathFromDiscovery: string,
  caseInsensitive = false,
): string | undefined {
  const indexed = normalizeRelativePath(indexedRoot);
  const workspace = normalizeRelativePath(workspacePathFromDiscovery);
  if (indexed === undefined || workspace === undefined) {
    return undefined;
  }
  if (!workspace) {
    return indexed;
  }
  return pathsEqual(workspace, indexed, caseInsensitive) ? '' : undefined;
}

export function indexedPath(
  workspaceRelativePath: string,
  indexedRootPrefix: string,
  caseInsensitive = false,
): string | undefined {
  const workspacePath = normalizeRelativePath(workspaceRelativePath);
  const prefix = normalizeRelativePath(indexedRootPrefix);
  if (workspacePath === undefined || prefix === undefined) {
    return undefined;
  }
  if (!prefix) {
    return workspacePath;
  }
  // Compare and split by segment, never by character offset: folding can change a string's length
  // (`ß` folds to `ss`), so a prefix length measured before folding would cut the wrong place.
  const prefixSegments = prefix.split('/');
  const segments = workspacePath.split('/');
  if (segments.length <= prefixSegments.length) {
    return undefined;
  }
  const head = segments.slice(0, prefixSegments.length).join('/');
  return pathsEqual(head, prefix, caseInsensitive)
    ? segments.slice(prefixSegments.length).join('/')
    : undefined;
}

/**
 * A document's path relative to its workspace folder, computed from the two `vscode.Uri.path`
 * values rather than through `vscode.workspace.asRelativePath`.
 *
 * `asRelativePath` answers with the HOST's separators, which is what forced a `\`→`/` rewrite
 * downstream and with it the ambiguity between a directory boundary and a backslash in a name.
 * URI paths carry no such ambiguity.
 *
 * Unlike [`indexedPath`], this takes no case argument. Both operands are CLIENT URIs and the
 * containment question has ALREADY been answered by `vscode.workspace.getWorkspaceFolder`, which
 * on a case-insensitive client matches a document opened through a differently-cased path — a
 * terminal link, a compiler diagnostic, any `Uri.file` built outside the explorer — to the folder
 * regardless of how that prefix is spelled. Re-deriving that answer exactly would silence every
 * lane for such a document, and nothing the user can set corrects a folder-prefix mismatch. The
 * head check therefore folds unconditionally: it cannot select a different folder, because there
 * is only the one VS Code handed in, so it is a guard against a malformed pair and not a decision.
 *
 * [`indexedPath`]'s prefix is the opposite case — it comes from the server or from the user, so a
 * differently-cased match there strips a prefix the client never confirmed and resolves the
 * remainder against another directory. That one stays exact unless the server is provably local.
 */
export function workspaceRelativePath(
  folderUriPath: string,
  documentUriPath: string,
): string | undefined {
  const folder = uriPathSegments(folderUriPath);
  const document = uriPathSegments(documentUriPath);
  if (document.length <= folder.length) {
    return undefined;
  }
  // Segment-wise, never by character offset — folding can change a string's length.
  const head = document.slice(0, folder.length).join('/');
  return pathsEqual(head, folder.join('/'), true)
    ? document.slice(folder.length).join('/')
    : undefined;
}

function uriPathSegments(uriPath: string): readonly string[] {
  const trimmed = uriPath.replace(/^\/+/, '').replace(/\/+$/, '');
  return trimmed ? trimmed.split('/') : [];
}

export function workspacePath(
  indexedRelativePath: string,
  indexedRootPrefix: string,
): string | undefined {
  const indexed = normalizeRelativePath(indexedRelativePath);
  const prefix = normalizeRelativePath(indexedRootPrefix);
  if (indexed === undefined || prefix === undefined) {
    return undefined;
  }
  return [prefix, indexed].filter(Boolean).join('/');
}

/**
 * Dotless `ı` uppercases to `I` in JavaScript, but Unicode case folding — what the server uses —
 * folds it to itself. It is the ONLY code point where the round trip below merges two names the
 * server keeps apart, measured by folding every code point with both rules.
 */
const DOTLESS_I = 'ı';

/**
 * Fold a path the way the server's canonical-path lookup does: NFC first, since a decomposed
 * `a` + U+0308 names the same file as a composed `ä` on a case-insensitive volume, then Unicode
 * case folding per code point.
 *
 * JavaScript has no case-folding primitive, so the fold is a lower→upper→lower round trip: the
 * leading lowercase is what lets `ẞ` reach `ß`'s `ss`, and the uppercase pass is what merges final
 * sigma `ς` with `σ`. Plain lowercasing does neither. Which direction the remaining inaccuracy
 * points is what matters: accepting a prefix the server would reject strips it and resolves the
 * remainder against a DIFFERENT directory, while rejecting one the server would accept only costs
 * signals for that file. Excluding [`DOTLESS_I`] leaves the relation a strict subset of the
 * server's, so only the harmless direction remains.
 */
function foldPath(path: string): string {
  return [...path.normalize('NFC')]
    .map((char) => (char === DOTLESS_I ? char : char.toLowerCase().toUpperCase().toLowerCase()))
    .join('');
}

function pathsEqual(left: string, right: string, caseInsensitive: boolean): boolean {
  return caseInsensitive ? foldPath(left) === foldPath(right) : left === right;
}
