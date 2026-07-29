# rag-rat Lens (VS Code / Cursor extension)

Read-only editor lens over the rag-rat repo index — issue #216. One shared
TypeScript source builds to both extension hosts: Node (desktop VS Code /
Cursor) and web worker (vscode.dev, Codespaces, Cursor web).

## Develop

```bash
npm install
npm run watch          # esbuild: dist/node + dist/web, rebuilds on change
```

Start the authenticated backend from the repository root:

```bash
rag-rat mcp
```

Then F5 ("Run Web Extension (webworker host)") opens an Extension Development Host; extension
code changes need a debug restart / Reload Window (no hot-swap in the host).

## Features (MVP)

- Status bar with live indexed-file count
- Clone-class overlays and extraction previews
- Memory CodeLens, hovers, diagnostics, and detail documents
- Call-graph, coupling, issue, and decision lenses
- Sidebar views for clone classes, memories, and tracker rationale
- SSE-driven refresh when the index or enrichment data changes

The extension discovers the loopback URL, bearer token, and indexed subdirectory from
`.rag-rat/sockets/lens.json` (local file-system workspaces only). You can open either the Git
worktree top or the configured indexed root; discovery walks trusted local ancestors and accepts
only the opened folder or its nearest Git worktree boundary. For a hosted server, run
**rag-rat Lens: Configure Server**; the URL is stored in settings and the bearer token in VS Code
SecretStorage. The same command records which repository AND which checkout that server serves for
the opened workspace — a repository's linked worktrees share one identity, and every line a server
reports belongs to the working tree it indexed — and asks for the indexed root: leave it empty to
follow the server's own metadata, or answer `.` when the opened folder is already the indexed
subdirectory. Spell that path with `/` on every platform, including Windows, and match the local
directory's casing exactly: a hosted server may run on a filesystem with the opposite case rules,
so the extension compares against the opened folder rather than guessing, and a mismatch shows no
signals instead of resolving against a different directory.
All of it is stored per (server, workspace) pair, so a second workspace or endpoint
never inherits the first one's mapping, and a server that moves to another checkout stops being
served rather than reporting another tree's lines.
Browser extension hosts (vscode.dev, Codespaces
web UI) also require their exact
Origin in the server's `--allow-origin` / `RAG_RAT_LENS_ORIGINS` allowlist, and Chrome requires
the server to opt into Private Network Access — the Rust server emits that header on preflights
from allowed origins.

Lens currently requires a single-root workspace. Desktop, remote, and virtual
workspace document schemes are supported; multi-root windows fail closed so
data from two repository servers cannot be mixed.
