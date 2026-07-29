export function normalizeRelativePath(path: string): string | undefined {
  const normalized = path.replaceAll('\\', '/');
  if (!normalized || normalized === '.') {
    return '';
  }
  if (normalized.startsWith('/') || /^[A-Za-z]:\//.test(normalized)) {
    return undefined;
  }
  const segments = normalized.split('/');
  if (segments.some((segment) => !segment || segment === '.' || segment === '..')) {
    return undefined;
  }
  return segments.join('/');
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
