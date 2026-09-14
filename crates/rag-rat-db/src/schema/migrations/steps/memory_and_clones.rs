use rusqlite::{Connection, params};

use crate::schema::migrations::{add_column_if_missing, column_exists};

/// V035: add `symbols.is_test` (cross-language test-code marker computed at parse time; see
/// `parser::detect_is_test`) so clone detection can keep tests out of the corpus. Idempotent via
/// `add_column_if_missing`. Existing rows default to 0 (non-test) until the next reindex
/// repopulates them — accurate `is_test` needs a reindex with this binary.
/// V048 (#465): `repo_memories.payload_json` — a nullable opaque canonical-JSON payload for
/// polymorphic memory nodes. The `Task` / `Concept` kinds carry a kind-specific,
/// `schema_version`-tagged payload here; the core stores it verbatim and folds its canonical form
/// into `content_hash` (`dream::note_content_hash`), so a payload edit self-invalidates the derived
/// dream summary/verdict rows exactly as a title/body edit does. Additive + nullable: existing rows
/// read back `NULL` (no payload).
pub(crate) fn apply_memory_payload_json(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "repo_memories", "payload_json", "TEXT")?;
    Ok(())
}

/// V049 (#464): `repo_node_edges` — the typed, content-addressed, cross-repo edge set. One row per
/// edge from a source memory NODE to a target that is either another node or a code/github anchor.
///
/// Design invariants baked into the shape:
///   - `edge_key` (PK) is the stable content-addressed identity: a `rebind` re-resolves the local
///     rowids WITHOUT changing the key, so a sync fold keeps presence/tombstones keyed by it.
///   - `repo_id` is the OWNER repo (the source node's repo — the periphery scope + adoption key);
///     `target_repo_id` is the target's repo, which MAY differ (a cross-repo edge into a sibling).
///   - Portable `target_kind` + `target_anchor` carry the durable target identity; the resolved
///     local rowids (`target_node_id` / `target_logical_symbol_id`) are re-derivable and carry NO
///     FK to volatile graph rows (the #248 rule — a reindex must never cascade-delete a durable
///     edge). The ONLY FK is `source_node_id` -> `repo_memories(id)` (a durable node), cascading so
///     an edge dies with its source.
///   - `anchor_status` is `current` | `gone` | `unresolved` (the last for a cross-repo target whose
///     repo is not present locally yet — re-resolved when it is indexed, never a hard failure).
pub(crate) fn apply_repo_node_edges(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS repo_node_edges(
            edge_key TEXT PRIMARY KEY,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            source_node_id TEXT NOT NULL,
            relation TEXT NOT NULL,
            target_repo_id TEXT NOT NULL,
            target_kind TEXT NOT NULL,
            target_anchor TEXT NOT NULL,
            target_node_id TEXT,
            target_logical_symbol_id INTEGER,
            symbol_kind TEXT,
            signature_hash TEXT,
            anchor_status TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(source_node_id) REFERENCES repo_memories(id) ON DELETE CASCADE
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_repo_node_edges_source
            ON repo_node_edges(source_node_id, relation);
        -- Reverse traversal (edges_into) matches on the globally-unique
        -- (target_kind, target_anchor) only, not target_repo_id (the anchor
        -- determines its repo), so the index leads with exactly those two columns.
        CREATE INDEX IF NOT EXISTS idx_repo_node_edges_target
            ON repo_node_edges(target_kind, target_anchor);
        ",
    )
}

/// V050 (#473): incremental clone-graph delta maintenance. The delta pass deletes a changed file's
/// postings by `(build_generation, path)` — unindexed until now (the PK leads with `token_hash`) —
/// and tracks how many files the live generation has absorbed since its full build
/// (`delta_files_applied`, the df-drift signal that schedules the next full rebuild). Both
/// additive + idempotent; the column type is STRICT-valid and defaulted so existing generation
/// rows read back 0 (no deltas absorbed).
///
/// Pre-freeze postings are NOT delta-ready (#477 review): binaries before the df epoch freeze
/// bumped `clone_token_df` on incremental passes WITHOUT invalidating the postings, so an
/// upgraded index can hold a live generation whose postings are ordered by an older df than the
/// current table — a delta patching it would compute sub-blocks under the moved df and silently
/// miss edges. Clear `postings_written` so those generations take one full rebuild (which re-pins
/// the epoch at its own build). Gated on the delta column being freshly ADDED, so only the first
/// run (a genuinely pre-freeze index) invalidates — a re-apply on an already-frozen index must
/// not throw away a valid graph.
pub fn apply_clone_delta_maintenance(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_clone_subblock_postings_path
             ON clone_subblock_postings(build_generation, path);",
    )?;
    // ORDER IS LOAD-BEARING (torn-retry safety): the invalidation runs BEFORE the gate column is
    // added. A kill between the two leaves the column absent, so the retry re-runs the
    // (idempotent) invalidation and then adds the column — the gate can never read "already
    // frozen" while the clear is still owed.
    if !column_exists(conn, "clone_graph_generations", "delta_files_applied")? {
        conn.execute_batch("UPDATE clone_graph_generations SET postings_written = 0;")?;
    }
    add_column_if_missing(
        conn,
        "clone_graph_generations",
        "delta_files_applied",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

/// V051 (#479): per-generation df snapshot. The #473 freeze made `clone_token_df` itself the
/// postings' frozen order, which left the LIVE candidate paths reading stale df — a newly indexed
/// token rides `DF_FALLBACK` (sorted last) until the next full build, so a new clone family's
/// sub-block prefixes prefer old tokens, and at the hot-token-cap margin its candidates drop
/// entirely. `clone_df_epoch` pins each generation's build-time df durably (CASCADE-swept with the
/// generation row, like edges/postings), so the persisted-graph consumers read their own build's
/// order while `clone_token_df` is free to move again on incremental passes.
///
/// BACKFILL (the V050→V051 bridge): a pre-epoch DB's servable generations were built under the
/// CURRENT `clone_token_df` — under the #473 freeze df cannot have moved since (any refresh
/// invalidated `postings_written`, and a postings-invalid Building generation is discarded, never
/// resumed) — so snapshotting current df per generation is exact and no forced rebuild is needed.
/// Backfill targets only generations with ZERO epoch rows (each per-generation INSERT is one
/// atomic statement, so a torn retry resumes cleanly), which also keeps a re-apply from folding
/// post-V051 (moving) df rows into an already-pinned epoch. Known degenerate edge: a generation
/// built while its repo's df table was empty backfills nothing; the runtime treats a missing
/// epoch like `postings_written = 0` (one self-healing rebuild).
pub fn apply_clone_df_epoch(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS clone_df_epoch(
             build_generation INTEGER NOT NULL REFERENCES clone_graph_generations(generation)
                                               ON DELETE CASCADE,
             token_hash       INTEGER NOT NULL,
             df               INTEGER NOT NULL,
             PRIMARY KEY (build_generation, token_hash)
         ) STRICT;",
    )?;
    // Enumerate the epoch-less generations FIRST, then snapshot each with its own atomic INSERT.
    // A single self-referencing `INSERT … WHERE NOT EXISTS(SELECT … FROM clone_df_epoch)` is not
    // used deliberately: SQLite may interleave the SELECT with the insertion, so rows landed for a
    // generation earlier in the same statement could suppress its remaining rows.
    let epoch_less: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT g.generation FROM clone_graph_generations g
             LEFT JOIN clone_df_epoch e ON e.build_generation = g.generation
             WHERE e.build_generation IS NULL
             GROUP BY g.generation",
        )?;
        stmt.query_map([], |row| row.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?
    };
    for generation in epoch_less {
        conn.execute(
            "INSERT INTO clone_df_epoch(build_generation, token_hash, df)
             SELECT g.generation, d.token_hash, d.df
             FROM clone_graph_generations g
             JOIN clone_token_df d
               ON d.repo_id = g.repo_id AND d.normalizer_kind = g.normalizer_kind
             WHERE g.generation = ?1",
            params![generation],
        )?;
    }
    Ok(())
}
