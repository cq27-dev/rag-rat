# Config Reference

`rag-rat.toml` has an `[index]` table, optional simple `[target_bindings]`, and optional richer `[[target]]` blocks.

`rag-rat init` writes a fully-commented `rag-rat.toml`: the lines reflecting your repo (bindings, embedding model) are active, and every other table below is emitted as commented defaults so the whole surface is discoverable in the file itself. For a C/C++ repo it detects C++ (any `.cpp`/`.cc`/… present) and binds the header directories as `cpp` so `.h` headers index as C++.

```toml
[index]
root = "."
# database is unset by default: the index + memories live in the machine-global store
# (see docs/config/database.md). Set it to keep this repo on its own file (deprecated):
# database = ".rag-rat/index.sqlite"

[llm.embedding]
# the MODEL by its full id (the HF path) — no aliases. "none" disables embeddings.
model = "sentence-transformers/all-MiniLM-L6-v2"

[llm.embedding.runtime]
batch_size = 64
ort_threads = 4
omp_threads = 1
max_embedding_chars = 4000
```

Each topic below has its own in-depth page under [`docs/config/`](config/):

- **[Database location](config/database.md)** — the machine-global consolidated store,
  per-repo opt-out, `rag-rat consolidate`, repo-identity requirements, and schema migrations.
- **[Embedding model](config/embedding.md)** — choosing the model (`[llm.embedding] model`),
  the registered models and their trade-offs, and the local runtime thread knobs
  (`[llm.embedding.runtime]`).
- **[Remote embedding](config/remote-embedding.md)** — serving the same model over
  Ollama/infinity/vLLM instead of in-process: connect mode, ephemeral cookbook GPU boxes,
  auto-tuned concurrency, and the `rag-rat init` cookbook catalog.
- **[Index targets](config/targets.md)** — `[target_bindings]` and `[[target]]` blocks,
  supported languages, the `.h` header C/C++ rule, and parser grammar pins.
- **[Index freshness](config/freshness.md)** — the background watcher (`[watch]`) and the
  generated git hooks that keep the index current as files and branches change.
- **[SCIP oracle auto-fresh](config/oracle.md)** — opt-in background compiler-grade ranking
  (`[oracle]`) with quiet-period and min-interval gates.
- **[Version check](config/version-check.md)** — the cached, fail-open crates.io update check
  (`[version_check]`).
- **[Dream memory maintenance](config/dream.md)** — the opt-in verify/compaction model
  (`[llm.dream]`), ephemeral GPU serving, nightly scheduling, and reviewing findings.
- **[Issue distillation model](config/distill.md)** — the opt-in LLM pass that fills distilled
  decision records (`[llm.distill]`), ephemeral GPU serving, and batching.
- **[Memory surfacing](config/memory.md)** — summary-vs-full rendering of repo memories
  everywhere they surface (`[memory] surface`).
- **[Issue trackers & papertrail](config/trackers.md)** — `[[tracker]]` bindings, per-provider
  notes and ref grammars, the quota reserve, sync cadences, and automatic synchronization.
- **[Peer sync](config/sync.md)** — replicating an account's memory op-log between devices
  (`[sync]`): choosing between the mesh and hub topologies, peer discovery and what it does and
  does not reveal, and the relay and cadence settings.
