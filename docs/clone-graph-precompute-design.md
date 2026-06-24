# Precomputed clone-edge graph for `find_clones`

Status: design approved (2026-06-24), implementation in progress on `feat/clone-graph-precompute`.
This doc is intentionally uncommitted — review artifact.

## Problem

`find_clones` recomputes a super-linear SourcererCC candidate-pair graph on every query
(`candidate_pairs_from_bags` → `sub_block_candidate_pairs` builds the inverted index in RAM and
emits the upper triangle of every token's posting list, then `verified_clone`). Fine at single-crate
scale (cargo: 3,332 functions, ~15s) but does **not** scale: the Linux kernel `drivers/net` (118k
functions) does not complete in 240s — candidate generation is the wall (CPU-bound, not OOM; it
stayed under a 22GB cap). Verified empirically (see repo memory `find_clones does NOT scale to
~118k functions`).

## Goal

Precompute and persist the **verified clone-edge graph** as a bounded, resumable **background** pass
(mirroring the embedding `reconcile`), so `find_clones` reads a persisted graph instead of
recomputing candidates. The pass cannot make candidate generation itself cheaper — it moves it off
the query path, bounds it by a time budget, and makes it resumable so a huge codebase converges over
several passes.

## Locked decisions

1. **Freshness = "mildly stale OK"** — eventually-consistent background recompute; `find_clones`
   serves the last *completed* graph (stale-serving), not incremental always-fresh maintenance.
2. **Persist depth = edges** (revised from "edges + postings" during implementation). A persisted
   token→symbol postings table would, under the #248 volatile-FK invariant (see Data model), itself
   have to be content-anchored — extra surface for a benefit only the incremental follow-up realizes.
   The resumable recompute instead rebuilds the in-RAM inverted index from the existing
   `symbol_fingerprints.token_bag` BLOBs each pass (cheap relative to pair emission). **Persisted
   postings are deferred to the incremental-maintenance follow-up.**
3. **Precompute at θ=0.7** (default `min_similarity`). Queries at θ>0.7 filter stored per-edge gate
   inputs (a 0.7 edge set is a superset of any θ≥0.7 set). Queries at θ<0.7 fall back to live.
4. **MCP is read-only** — writers are the watcher maintenance pass, the git-hook `maintenance`
   command, and explicit `rag-rat clones --precompute`.
5. **Fast path, never a correctness dependency** — graph absent / no completed generation / θ<0.7 /
   non-base scope → today's live path, unchanged.
6. **MVP scope = base index only**; worktree overlays fall back to live (follow-up).

## Data model (migration V034 — implemented)

Generation-staged so a half-built graph is never served: the recompute writes a new
`build_generation`; reads serve the latest **Complete** generation; the live pointer flips
atomically on completion.

**Endpoints are content-anchored, NOT `symbol_id`.** A repo invariant (the #248 bug class, enforced
by the `no_table_has_a_reindex_cascading_fk_to_a_volatile_parent` trip-wire) forbids a durable table
from carrying an `ON DELETE CASCADE` FK to a `REINDEX_VOLATILE_PARENT` (`symbols`, `files`,
`edges_data`, `logical_symbols`) — `symbol_id` is reassigned on every reindex, so keying on it is the
exact bug that silently wiped `edge_oracle` verdicts. `clone_edges` therefore anchors each endpoint
on the reindex-stable `(path, start_byte)` + the `file_sha` (`files.sha256`) at compute time — the
same content-key/staleness pattern as `edge_oracle`. Reads resolve an endpoint by joining live
`symbols`/`files` on `(path, start_byte)` AND `files.sha256 = *_file_sha`; a deleted or edited
endpoint simply does not resolve, so a dangling/stale edge is dropped at read (never a ghost). The
`build_generation` FK targets `clone_graph_generations`, which is **durable** precompute metadata
(not volatile), so that CASCADE is allowed and powers generation GC.

```sql
CREATE TABLE clone_graph_generations(
    generation         INTEGER PRIMARY KEY,
    status             TEXT    NOT NULL CHECK (status IN ('Building','Complete')),
    theta_floor        REAL    NOT NULL,   -- 0.7
    normalizer_kind    TEXT    NOT NULL,   -- 'baseline'
    normalizer_version INTEGER NOT NULL,   -- NORM_VERSION at build
    source_revision    TEXT    NOT NULL,   -- content_revision() this generation builds toward
    cursor_symbol_id   INTEGER NOT NULL DEFAULT 0,  -- build-local resume point
    edges_written      INTEGER NOT NULL DEFAULT 0,
    started_at_ms      INTEGER NOT NULL,
    finished_at_ms     INTEGER
) STRICT;

CREATE TABLE clone_edges(
    build_generation INTEGER NOT NULL REFERENCES clone_graph_generations(generation) ON DELETE CASCADE,
    a_path           TEXT    NOT NULL,     -- content anchor (canonical a < b by path,start_byte)
    a_start_byte     INTEGER NOT NULL,
    a_file_sha       TEXT    NOT NULL,     -- files.sha256 at compute; read-time staleness filter
    b_path           TEXT    NOT NULL,
    b_start_byte     INTEGER NOT NULL,
    b_file_sha       TEXT    NOT NULL,
    overlap          INTEGER NOT NULL,     -- exact verified_clone gate inputs (theta>=0.7 re-filters)
    a_token_len      INTEGER NOT NULL,
    b_token_len      INTEGER NOT NULL,
    similarity       REAL    NOT NULL,     -- overlap/max_len; 1.0 for struct-hash-exact pairs
    edge_source      TEXT    NOT NULL,     -- 'struct_hash' | 'sub_block'
    PRIMARY KEY (build_generation, a_path, a_start_byte, b_path, b_start_byte)
) STRICT;
CREATE INDEX idx_clone_edges_b ON clone_edges(build_generation, b_path, b_start_byte);
-- (clone_subblock_postings deferred: the in-RAM index is rebuilt from symbol_fingerprints.token_bag
--  each pass; a persisted, content-anchored postings table lands with incremental maintenance.)
```

Meta key (in `index_meta`, via `set_meta`): `clone_graph_live_generation` → the generation the read
path serves (the newest `Complete`).

## Recompute pass (resumable, generation-staged)

New module `index/clones/reconcile.rs`, mirroring `index/ai/reconcile.rs`. Entry:
`reconcile_clone_edges(conn, CloneEdgeOptions{max_seconds, batch_size, force}, progress) ->
CloneEdgeReport`.

- **Phase 0 — skip-when-current.** If the latest `Complete` generation's `source_revision ==
  content_revision()` and `normalizer_version == NORM_VERSION` and `!force` → return `Current`,
  zero writes (the `model_manifest_is_current` no-op pattern; keeps idle passes free).
- **Phase 1 — open/resume a building generation.** Reuse an existing `Building` row (resume at its
  `cursor_symbol_id`) or allocate `generation = max+1`, `status='Building'`, `source_revision =
  content_revision()` snapshot.
- **Phase 2 — refresh postings** for the building generation (stream fingerprint BLOBs → emit
  `(sub_block_token → symbol_id)` rows). Chunkable by symbol-id cursor.
- **Phase 3 — gen edges (the streaming SourcererCC form).** Walk symbols in `symbol_id` ASC from
  `cursor_symbol_id + 1`. For each symbol `s`: its sub-block tokens → query `clone_subblock_postings`
  for partners `t > s` only (smaller-endpoint partition → each unordered pair emitted once, dedup is
  structural) → `verified_clone(B_s, B_t, 0.7)` → on pass insert `(s, t, overlap, token_lens,
  similarity, struct_hashes)`. Also emit the struct-hash exact pairs for `s` (`similarity = 1.0`).
  Batch by `batch_size`; per-symbol (not just per-batch) budget check; checkpoint `cursor_symbol_id`
  after each batch in one txn with that batch's edge inserts.
- **Completion.** When the walk reaches the last symbol: set the generation `status='Complete'`,
  `finished_at_ms`, `edges_written`, then set `clone_graph_live_generation = generation` and delete
  superseded generations (CASCADE drops their edges/postings) — the live-pointer flip is the **last**
  write (publish-after-build, so readers see old-Complete or new-Complete, never Building).
- **Budget trip.** Persist the cursor, leave `status='Building'`, return `Partial`. Next pass resumes.
- **Mid-build reindex** (between passes): CASCADE may drop some staging edges the cursor already
  passed → the completed generation has *false negatives* (missing edges), never wrong edges. Under
  "mildly stale OK" this is acceptable; the next clean pass (after edits settle) repairs them.

`CloneEdgeReport`/`CloneEdgeProgress` mirror `ReconcileReport`/`ReconcileProgress`.

## `find_clones` read path

One branch in `find_clones` (and `clones_for_symbol`) replaces how `pairs` is sourced; everything
downstream is unchanged.

```rust
let pairs = match precomputed_pairs_if_eligible(conn, &by_id, theta)? {
    Some(pairs) => pairs,                       // FAST
    None        => candidate_pairs_from_bags(&bags, theta),  // LIVE (unchanged)
};
```

`precomputed_pairs_if_eligible` returns `Some` iff: θ ≥ 0.7, a `Complete` generation exists with
`normalizer_version == NORM_VERSION`, and base scope. It then, inside one read transaction:
1. loads `clone_edges` for `clone_graph_live_generation`;
2. **resolves + validates each endpoint** by joining live `symbols`/`files` on `(path, start_byte)`
   AND `files.sha256 = *_file_sha` (→ a live `symbol_id`); an endpoint that doesn't resolve
   (deleted / edited file) drops the edge (count them as `graph_edges_dropped_stale`);
3. **scope-filters** to pairs whose both resolved endpoints are in `by_id`;
4. **θ-filters**: keep iff `overlap >= ceil(theta * max(a_token_len, b_token_len))` (exact parity
   with `verified_clone`; struct-hash pairs have `overlap == token_len` → similarity 1.0 → always
   survive).

Downstream `components_from_pairs` → `coherence_split` (fed the stored `similarity`) → `build_class`
→ refine (already cached) runs unchanged. Stale-serving: a present-but-stale live generation is still
served; `CloneCompleteness` gains `served_from: "precomputed" | "live"`, `graph_source_revision`, and
`graph_edges_dropped_stale`, so staleness is honest. Member hydration safety is already handled by
`build_class`'s existing TOCTOU defenses + the endpoint validation in (2).

## Freshness model

`graph_fresh` = `clone_graph_live_generation` row exists AND `source_revision == content_revision()`
AND `normalizer_version == NORM_VERSION`. `content_revision` (files-sha digest) + the explicit
`normalizer_version` pin together catch the common edits and a NORM bump (which `content_revision`
alone misses). A generated-flag flip without a file edit is a known residual gap → full
fingerprint-digest freshness is a follow-up if it ever bites. Staleness never blocks reads (locked).

## Triggers / wiring

All three writers share one `ReconcileBudget` (embeddings claim first, clone-edges take the
remainder), so a pass stays within one `PASS_RECONCILE_MAX_SECONDS`:
- **`watch.rs::run_pass`** — sibling step after the base embedding reconcile, inside the existing
  idle-backstop guard, gated by `pending_clone_graph()` (absent or stale).
- **`commands/mod.rs::run_maintenance_pass`** (git-hook) — same insertion; clone report joins the
  JSON under `clone_graph`.
- **`rag-rat clones --precompute [--max-seconds N]`** — explicit foreground writer (RW open under
  `WriteLock`), loops passes to `Complete`.
- **Full rebuild does NOT force-complete edges** (would regress index time per #251/#285 lessons);
  CASCADE leaves the graph consistent, background passes fill it.

## Status / CLI surface

`status()` gains `clone_graph_fresh: bool`, `clone_graph_source_revision`, `clone_graph_built_at_ms`,
`clone_graph_edge_count` (read from the live generation row — NOT a `COUNT(*)`; honors the #285
"status stays cheap" rule). `clones --precompute` prints a `CloneEdgeReport`.

## Module layout / files touched

- `index/schema/migrations.rs` + `mod.rs` — V034 DDL const + `apply_*` fn; consts + 3 match arms +
  `ADDITIVE_MIGRATIONS` entry; bump `LATEST_SCHEMA_VERSION` to 34.
- `index/clones/reconcile.rs` (new) — `reconcile_clone_edges`, options/report/progress, the streaming
  build, checkpoint/resume.
- `index/clones/edges_store.rs` (new) — `clone_edges`/`clone_subblock_postings`/generation read+write
  helpers, `precomputed_pairs_if_eligible`, `pending_clone_graph`, freshness getters.
- `index/clones/mod.rs` — add the two modules to the curated index.
- `query_api/clones.rs` — the fast-path branch in `find_clones` + `clones_for_symbol`; `CloneCompleteness`
  provenance fields; `CLONE_PRECOMPUTE_THETA`.
- `query_api/mod.rs` + `index/mod.rs` — status fields (cheap reads).
- `watch.rs`, `commands/mod.rs`, `cli.rs` — trigger wiring + CLI flag.

## Testing

1. **Parity (cornerstone)** — on the cargo fixture, `find_clones` via precomputed graph ==
   via live path, byte-identical `recall_signature`, for θ ∈ {0.7, 0.8, 0.9} × several `min_copies`.
2. **Resume** — tiny `max_seconds` trips `Partial` mid-walk; resume to `Complete`; final edge set ==
   one uninterrupted pass (proves the smaller-endpoint partition is correct across a checkpoint).
3. **CASCADE / stale** — delete a symbol's file / reindex; dependent edges vanish; no dangling
   hydrate. Endpoint struct_hash mismatch drops the edge + increments `graph_edges_dropped_stale`.
4. **Skip-when-current** — second pass takes zero write locks; no generation row added.
5. **Fallback** — θ<0.7, no Complete generation, wrong norm_version, overlay scope → live path,
   result == today.
6. **Budget sharing** — embeddings first, clone-edges remainder; near-exhausted budget skips cleanly.
7. **Schema** — V034 fresh-DB + forward-migrate + checksum-stable; `CHECK(a<b)` rejection.

## Phasing

- **A — substrate**: V034 migration (3 tables + meta) + `edges_store.rs` skeleton + status fields.
  No read-path change; ships safe.
- **B — recompute**: `reconcile.rs` streaming build + `clones --precompute` CLI. Writer only.
- **C — read path**: `precomputed_pairs_if_eligible` + the two splices + parity test. The behavior
  change, gated on parity.
- **D — auto-maintenance**: `watch.rs` + `maintenance` budgeted sibling step.

## Non-goals / follow-ups

Incremental delta maintenance (always-fresh); worktree-overlay graphs; θ<0.7 coverage; full
fingerprint-digest freshness; serving built/ranked classes (only candidate gen is precomputed —
`coherence_split`/`build_class`/refine stay live, bounded by #272/#282 + `min_copies`).
