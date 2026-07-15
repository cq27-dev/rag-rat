# Embedding model (`[llm.embedding]`)

Part of the [config reference](../config.md).

Selects how `semantic_search` computes the **vector** half of its hybrid ranking. The selector is the
model's **full id — its HF path** (no aliases). `rag-rat init` recommends a default from repo size;
you can override it here. The registered models:

- `sentence-transformers/all-MiniLM-L6-v2` (default) — MiniLM transformer via FastEmbed (384-dim).
  Best retrieval quality, but the cold backfill is CPU-bound (~10-100 chunks/sec), so it's only
  comfortable for repos that finish in a few minutes.
- `BAAI/bge-small-en-v1.5` — a stronger general-retrieval transformer at the same 384-dim.
- `jinaai/jina-embeddings-v2-base-code` — a code-specific transformer (768-dim).
- `minishlab/potion-retrieval-32M` — static token-vector lookup + mean-pool (512-dim). ~100-500×
  faster on CPU at some retrieval-quality cost: distributional/synonym semantics but no context, word
  order, or polysemy disambiguation. The right choice for large repos that still want vectors. BM25
  (the other half of the hybrid) cushions the quality drop.
- `none` — structural + BM25 only; no dense vectors. `semantic_search` degrades to BM25, and every
  other tool (symbols, graph, impact, git/papertrail, memories) is unaffected. The cheapest option
  for enormous codebases (e.g. the Linux kernel) where any embedding backfill is impractical.

The selector chooses which model `init` installs and activates. The active model is recorded in the
index, so switching the model in the config takes effect after re-running `rag-rat init` or
`rag-rat models install <model-id>` (and a reconcile to re-embed under the new model). Different
models have different vector dimensions, so switching re-embeds from scratch.

## Local runtime (`[llm.embedding.runtime]`)

`[llm.embedding.runtime]` controls reconcile defaults for local embedding generation. CLI flags
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
