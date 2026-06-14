# Config Reference

`rag-rat.toml` has an `[index]` table, optional simple `[target_bindings]`, and optional richer `[[target]]` blocks.

```toml
[index]
root = "."
database = ".rag-rat/index.sqlite"

[local_ai.embedding]
model = "minilm"   # embedding backend: "minilm" | "model2vec" | "none"

[local_ai.embedding.runtime]
batch_size = 64
ort_threads = 4
omp_threads = 1
max_embedding_chars = 4000
```

## Embedding backend (`[local_ai.embedding] model`)

Selects how `semantic_search` computes the **vector** half of its hybrid ranking. `rag-rat init`
recommends a default from repo size; you can override it here.

- `minilm` (default) — MiniLM transformer via FastEmbed. Best retrieval quality, but the cold
  embedding backfill is CPU-bound (~10-100 chunks/sec), so it's only comfortable for repos that
  finish in a few minutes.
- `model2vec` — static token-vector lookup + mean-pool (`minishlab/potion-retrieval-32M`, 512-dim).
  ~100-500× faster on CPU at some retrieval-quality cost: it has distributional/synonym semantics
  but no context, word order, or polysemy disambiguation. The right choice for large repos that
  still want vectors. BM25 (the other half of the hybrid) cushions the quality drop.
- `none` — structural + BM25 only; no dense vectors. `semantic_search` degrades to BM25, and every
  other tool (symbols, graph, impact, git/papertrail, memories) is unaffected. The cheapest option
  for enormous codebases (e.g. the Linux kernel) where any embedding backfill is impractical.

The selector chooses which model `init` installs and activates. The active model is recorded in the
index, so switching the backend in the config takes effect after re-running `rag-rat init` or
`rag-rat models install <model-id>` (and a reconcile to re-embed under the new model). Different
backends have different vector dimensions, so switching re-embeds from scratch.

The database stores explicit schema migrations in `schema_version` with migration id,
`applied_at_ms`, checksum, and description. Opening the index **migrates an older schema forward
automatically** — the migration ladder is additive and idempotent, so a binary upgrade needs no
manual step. Only a *newer* schema (created by a future rag-rat — can't downgrade), a *dirty* or
checksum-mismatched schema (rebuild with `rag-rat index --full`), or a *missing* index (build it
first) is refused. `rag-rat doctor` reports the schema state without changing anything.

Simple bindings map a language to directories:

```toml
[target_bindings]
rust = ["core/held-core/src"]
typescript = ["apps/mobile/src"]
kotlin = ["apps/wear-bridge/src"]
markdown = ["docs"]
```

Expanded targets add name, kind, include, and exclude metadata:

```toml
[[target]]
name = "held-core-generated-bindings"
language = "typescript"
directories = ["packages/held-core/src/generated"]
kind = "generated"
include = ["**/*.ts"]
exclude = ["**/*.map"]
```

Supported languages are `rust`, `typescript`, `kotlin`, and `markdown`. Rust, TypeScript/TSX,
and Kotlin source use tree-sitter structural indexing when files are under the parser size cap.
Markdown uses heading-section chunking and does not use tree-sitter. Supported target kinds are
`source`, `generated`, `docs`, and `tests`; generated targets are indexed with coarse chunks and
still obey `include_generated` filtering.

Parser grammar dependencies are exact-pinned in `Cargo.toml`: `tree-sitter` 0.22.6,
`tree-sitter-rust` 0.21.2, `tree-sitter-typescript` 0.21.2, and `tree-sitter-kotlin` 0.3.8.

`[local_ai.embedding.runtime]` controls reconcile defaults for local embedding generation. CLI flags
still take precedence: `--batch-size` overrides `batch_size`, and `--max-embedding-chars` overrides
`max_embedding_chars`.

Thread controls:

- `ort_threads` caps the ONNX Runtime **intra-op** thread pool, applied through fastembed's session
  (`with_intra_threads`). **Caveat:** the prebuilt ONNX Runtime binaries fastembed downloads are
  Microsoft's OpenMP builds, where the intra-op setting has no effect — so on the default binaries
  this knob is inert and `omp_threads` is the one that matters.
- `omp_threads` is exported as the `OMP_NUM_THREADS` environment variable (only when not already set
  by the caller). For the OpenMP prebuilt binaries this is **the** effective embedding-thread lever.
  Note the default is `1`, which makes embedding single-threaded; raise it (e.g. to your core count)
  for faster reconciliation on multi-core machines.

(`ort_threads` is no longer exported as `ORT_NUM_THREADS` — ONNX Runtime does not read that
variable.)

`[watch]` controls the background file watcher that keeps the index fresh as files change (new
files, uncommitted edits) so graph/symbol queries reflect the working tree without a commit:

```toml
[watch]
enabled = true        # on by default; false (or RAG_RAT_NO_WATCH=1) disables it
debounce_ms = 400     # quiet window before a reindex pass
max_latency_ms = 2500 # force a pass after this much continuous activity (starvation cap)
periodic_sweep_secs = 300 # backstop pass at least this often (0 disables) — set for NFS/WSL
```

The watcher runs inside `rag-rat mcp` automatically, and on demand via `rag-rat index --watch`. It
watches the configured target directories recursively and runs the discover → reconcile → gc →
memory_validate pipeline on debounced bursts. One watcher per worktree and one writer at a time per
index are enforced with file locks under the index directory; the index DB is shared across a repo's
worktrees (a relative `database` resolves against the main worktree). File locks are unreliable on
NFS and WSL2 `drvfs`/`9p` (`/mnt/...`) mounts — keep the repo on a native filesystem.

`[version_check]` controls the best-effort check for a newer published `rag-rat` on crates.io,
surfaced to agents/operators in the SessionStart digest and the `index_status` MCP tool's `version`
field (current vs latest + the `cargo install rag-rat --force` update command):

```toml
[version_check]
enabled = true   # opted in by default; false makes zero network calls
```

The check is **cached** (refreshed at most once a day, out of band by the long-lived `rag-rat mcp`
server) and **fail-open** — offline, a non-200, or a parse miss simply yields no version info, never
an error, and **never blocks** session start (reads only the cache; `rag-rat version-check` refreshes
it synchronously on demand). The cache lives at `<index-dir>/version-check.json`. Set
`enabled = false` to disable the feature entirely (no crates.io requests).

`rag-rat hooks install` writes generated `post-checkout`, `post-merge`, `post-rewrite`, and
`post-commit` hooks to the current worktree's Git hooks directory. Those hooks call `rag-rat
maintenance --max-seconds 30` in the background so branch switches, merges, rebases, and commits
refresh the current worktree index and advance changed-first embedding reconciliation without
blocking normal Git operations. Each maintenance pass also runs a worktree-safe `gc` that prunes
index rows for commits no longer held by any live worktree (run `rag-rat gc` to prune on demand).
