# Database location (`[index] database`)

Part of the [config reference](../config.md).

By default rag-rat keeps **one consolidated database per machine** — `$XDG_DATA_HOME/rag-rat/rag-rat.sqlite`
(falling back to `~/.local/share/rag-rat/rag-rat.sqlite`, or `%APPDATA%\rag-rat\rag-rat.sqlite` on
Windows) — holding every registered repo's index and, most importantly, its **repo memories**. This
means an accidental `git clean -fdx` or a deleted checkout no longer takes your authored memories
with it. The location is resolved by cascade:

1. `RAG_RAT_DATA_DIR` — an explicit override, honored verbatim.
2. `$XDG_DATA_HOME/rag-rat`.
3. `$HOME/.local/share/rag-rat` (the XDG default).
4. (Windows) `%APPDATA%\rag-rat`.

Set an explicit `database` key to opt a repo **out** of the consolidated store and keep it on its own
per-repo file:

```toml
[index]
database = ".rag-rat/index.sqlite"   # deprecated: this repo stays un-consolidated
```

An explicit key is honored for backward compatibility but is **deprecated** — an un-consolidated repo
does not share the machine's global store (and, later, cannot participate in memory sync). A relative
path resolves against the **main worktree top**, so every worktree of a repo shares one index; an
absolute path is used as-is.

**Upgrading an existing repo.** A repo indexed before this default flip has a legacy
`.rag-rat/index.sqlite`. rag-rat keeps using it (with a one-line deprecation notice) until you run:

```
rag-rat consolidate
```

which imports the legacy index's memories and content-addressed embedding cache into the global store,
renames the old file to `index.sqlite.imported` (delete it at your leisure), and prints a summary.
Consolidation is idempotent, and everything else in the legacy file is rebuilt on the next
`rag-rat index --full` (the carried embedding cache makes re-embedding a no-op).

If `rag-rat.toml` pins an explicit `database` key, consolidation refuses outright (nothing is
imported): remove the key first, then run `rag-rat consolidate` — the single run imports and renames
atomically, so there is never a window in which memories written to the still-live legacy file could
be lost. A pin at a **custom** path additionally needs its file moved to `.rag-rat/index.sqlite`
first (a keyless config never consults a custom path); the refusal prints the exact commands for
your paths. Once the `.imported` marker exists it acts as a stay-global latch — a stray legacy file that
later reappears beside it is ignored (with a warning), never silently adopted.

The global default also requires a **resolvable repo identity** — a git repository with at least one
commit, or an `[index] repo_id` pin. A root without one (a non-git directory, a fresh `git init`
with no commits yet) keeps its per-root `.rag-rat/index.sqlite` exactly as before the flip, so
unrelated identity-less projects never share scope in the global store; once the repo gains a first
commit (or a pin), the default resolves globally and `rag-rat consolidate` migrates it.

## Schema migrations

The database stores explicit schema migrations in `schema_version` with migration id,
`applied_at_ms`, checksum, and description. Opening the index **migrates an older schema forward
automatically** — the migration ladder is additive and idempotent, so a binary upgrade needs no
manual step. Only a *newer* schema (created by a future rag-rat — can't downgrade), a *dirty* or
checksum-mismatched schema (rebuild with `rag-rat index --full`), or a *missing* index (build it
first) is refused. `rag-rat doctor` reports the schema state without changing anything.
