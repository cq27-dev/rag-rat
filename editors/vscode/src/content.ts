// Content agreement between the index and the editor: the hash a per-file answer's
// `content_sha256` is compared against before anything line-anchored is drawn.
import * as vscode from 'vscode';
import { logError } from './output';
import { sha256Hex } from './sha256';

/**
 * Documents whose digest is remembered at once. Bounded for the same reason the file store is: an
 * editing session across a large repository visits far more files than it ever looks at twice.
 */
const MAX_CACHED_DIGESTS = 64;

/**
 * A file's content hash together with the document version it was taken for.
 *
 * The version is not bookkeeping — it is what makes the hash usable as evidence. A digest is
 * produced across two awaits, and a document can be RELOADED across them (an external write to a
 * clean buffer) without going dirty and without changing its path, so the bytes hashed need not be
 * the bytes on screen by the time the answer is used. Naming the version the digest speaks for
 * lets the consumer check that the premise still holds, exactly as it re-checks the path.
 */
export interface DocumentDigest {
  version: number;
  /** Lower-hex SHA-256 of the file's raw bytes, in the same space as the index's `files.sha256`. */
  sha256: string;
}

interface CachedDigest {
  version: number;
  sha256: string;
}

interface PendingDigest {
  version: number;
  /** The invalidation count this computation started under — see [`forget`](DocumentDigests#forget). */
  generation: number;
  digest: Promise<DocumentDigest | undefined>;
}

/**
 * Hex SHA-256 of the bytes an indexed file holds, remembered per document version.
 *
 * The bytes come from `workspace.fs`, not from `document.getText()`, and that is the whole
 * correctness argument. The index stores `sha256(the file's raw bytes)`: a UTF-8 BOM is part of
 * them, and so are CRLF line endings. A document's text is the DECODED buffer — VS Code strips the
 * BOM from it, and re-encoding it yields different bytes for any file the editor did not decode as
 * UTF-8. Hashing that would disagree with the server for every file of those shapes, forever, and
 * a permanent silence is a worse outcome than the skew the comparison exists to catch. Reading the
 * file costs nothing in correctness here because the gate only ever runs on a SAVED document — a
 * dirty buffer has no indexed path at all — so the file on disk is exactly what the editor shows.
 *
 * `workspace.fs` is also the host-neutral choice: it works on desktop, remote, and virtual
 * workspaces alike, which is where hosted servers are most likely to be used in the first place.
 *
 * Keyed by document version so the cost is one read + one hash per SAVE, not per refresh: version
 * moves on every content change, including the reload VS Code performs when a file changes
 * underneath it. A URI and a version do NOT identify a document across a close and reopen, though
 * — a reopened document commonly starts at the version its predecessor had — so what may be
 * REMEMBERED is additionally gated on the invalidation count `forget` moves.
 */
export class DocumentDigests {
  /** Least-recently-used last: a hit re-inserts, and the oldest entry is evicted. */
  private readonly digests = new Map<string, CachedDigest>();
  /**
   * Digests being computed right now, so overlapping readers share one read and one hash. The
   * same document is asked about by its editor's overlays and diagnostics, both CodeLens
   * providers, the hover and the three sidebar views, and it can be open in split editors at once
   * — on a remote workspace every duplicate is another network read of the whole file.
   */
  private readonly inflight = new Map<string, PendingDigest>();
  /** Moved by every `forget`; see there for what it invalidates. */
  private generation = 0;

  /**
   * The digest for `document`, or `undefined` when it cannot be produced here — which now means
   * only that the file could not be read.
   *
   * `undefined` deliberately reads as "no comparison is possible", never as "they disagree". A
   * host that cannot hash falls back to the behaviour it had before content agreement existed;
   * treating it as a mismatch would blank every surface on that host permanently. A digest that
   * no longer describes the document is a different answer, and the caller tells them apart by
   * the version this one carries.
   */
  async of(document: vscode.TextDocument): Promise<DocumentDigest | undefined> {
    const key = document.uri.toString();
    const version = document.version;
    const cached = this.digests.get(key);
    if (cached?.version === version) {
      this.digests.delete(key);
      this.digests.set(key, cached);
      return { version, sha256: cached.sha256 };
    }
    const pending = this.inflight.get(key);
    if (pending?.version === version && pending.generation === this.generation) {
      return await pending.digest;
    }
    const started: PendingDigest = {
      version,
      generation: this.generation,
      digest: this.take(document, key, version, this.generation),
    };
    this.inflight.set(key, started);
    try {
      return await started.digest;
    } finally {
      if (this.inflight.get(key) === started) {
        this.inflight.delete(key);
      }
    }
  }

  /**
   * Drop what is known about `uri`, and stop a computation already running for it from being
   * remembered.
   *
   * Deleting the entry is not enough on its own. A read started before this call completes after
   * it, holding a document object whose version still reads as the captured one — and a document
   * reopened at the same URI commonly starts at that same version, so the completion would hand
   * the reopened document the closed one's hash and let it certify line anchors taken before the
   * file changed. The counter is what such a completion checks to learn it was overtaken.
   */
  forget(uri: vscode.Uri): void {
    const key = uri.toString();
    this.digests.delete(key);
    this.inflight.delete(key);
    this.generation += 1;
  }

  private async take(
    document: vscode.TextDocument,
    key: string,
    version: number,
    generation: number,
  ): Promise<DocumentDigest | undefined> {
    let bytes: Uint8Array;
    try {
      bytes = await vscode.workspace.fs.readFile(document.uri);
    } catch (error) {
      logError(`content hash ${key}`, error);
      return undefined;
    }
    // Web Crypto where the host has it, and this extension's own implementation where it does not
    // — an extension host on Node 18, or a browser host outside a secure context. The gate has to
    // be ON everywhere `engines.vscode` says this extension runs: a host that silently cannot hash
    // would keep drawing another revision's line numbers with nothing to signal it. See sha256.ts.
    //
    // `workspace.fs.readFile` is typed over any buffer kind, and Web Crypto declines a SHARED one
    // — which a filesystem read never returns. Narrowed rather than copied: the alternative is a
    // second full-size allocation per file, which is the one cost worth avoiding here.
    const subtle = globalThis.crypto?.subtle;
    const sha256 = subtle
      ? hex(new Uint8Array(await subtle.digest('SHA-256', bytes as BufferSource)))
      : sha256Hex(bytes);
    // The read describes the bytes on disk when it happened, which is what the server's hash
    // describes too — but it may no longer describe THIS document, so it is remembered only while
    // both premises it was taken under still hold: the same version, and no `forget` in between.
    // It is still returned, labelled with the version it speaks for, so a caller can tell a digest
    // about the buffer in front of it from one about whatever the buffer used to hold.
    if (this.generation === generation && document.version === version) {
      this.remember(key, { version, sha256 });
    }
    return { version, sha256 };
  }

  private remember(key: string, entry: CachedDigest): void {
    this.digests.delete(key);
    this.digests.set(key, entry);
    while (this.digests.size > MAX_CACHED_DIGESTS) {
      const oldest = this.digests.keys().next().value;
      if (oldest === undefined) {
        break;
      }
      this.digests.delete(oldest);
    }
  }
}

/** Lower-hex, matching the server's rendering of `files.sha256`. */
function hex(bytes: Uint8Array): string {
  let out = '';
  for (const byte of bytes) {
    out += byte.toString(16).padStart(2, '0');
  }
  return out;
}
