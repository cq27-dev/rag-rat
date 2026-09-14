use rusqlite::Connection;

use crate::schema::migrations::add_column_if_missing;

/// V029 (#215, rework R1): clone-detection fingerprint substrate. All tables are scope-INDEPENDENT
/// — symbol_fingerprints/symbol_token_postings key by symbol_id (FK CASCADE discards them on
/// reindex); clone_token_df is a derived selectivity cache; clone_refinements keys by content
/// (class_key). NEVER add scope columns here (see mem_19ec7384a9b).
///
/// R1 replaces the MinHash/LSH fingerprint_bands table with a SourcererCC-style inverted-index
/// pair: symbol_token_postings (per-symbol token bag) + clone_token_df (document-frequency cache).
pub(crate) const CLONE_FINGERPRINT_DDL: &str = "
    CREATE TABLE IF NOT EXISTS symbol_fingerprints(
        symbol_id          INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
        normalizer_kind    TEXT    NOT NULL,            -- baseline | scip
        normalizer_version INTEGER NOT NULL,
        oracle_run_id      INTEGER,                     -- NULL for baseline rows
        struct_hash        TEXT    NOT NULL,
        token_len          INTEGER NOT NULL,
        created_at_ms      INTEGER NOT NULL,
        PRIMARY KEY (symbol_id, normalizer_kind)
    ) STRICT;
    CREATE INDEX IF NOT EXISTS idx_symbol_fingerprints_struct
        ON symbol_fingerprints(normalizer_kind, struct_hash);
    CREATE TABLE IF NOT EXISTS symbol_token_postings(
        symbol_id       INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
        normalizer_kind TEXT    NOT NULL,
        token_hash      INTEGER NOT NULL,              -- FNV-1a(token) as signed i64
        freq            INTEGER NOT NULL,
        PRIMARY KEY (symbol_id, normalizer_kind, token_hash)
    ) STRICT;
    -- Plan-1 candidate read loads postings by symbol_id (the PK prefix) and builds the inverted
    -- index in Rust; a token_hash secondary index is unused. Plan 2 re-adds one if
    -- clones_for_symbol moves the reverse lookup to SQL.
    CREATE TABLE IF NOT EXISTS clone_token_df(
        normalizer_kind TEXT    NOT NULL,
        token_hash      INTEGER NOT NULL,
        df              INTEGER NOT NULL,
        PRIMARY KEY (normalizer_kind, token_hash)
    ) STRICT;
    CREATE TABLE IF NOT EXISTS clone_refinements(
        class_key               TEXT    PRIMARY KEY,
        language                TEXT    NOT NULL,
        refine_mode             TEXT    NOT NULL,        -- baseline | scip
        template                TEXT    NOT NULL,
        variation_points_json   TEXT    NOT NULL CHECK (json_valid(variation_points_json)),
        proposed_signature_json TEXT    NOT NULL CHECK (json_valid(proposed_signature_json)),
        confidence              TEXT    NOT NULL,
        anti_unify_coverage     REAL    NOT NULL,
        lcs_ratio               REAL    NOT NULL,
        refactorability         REAL    NOT NULL,
        norm_version            INTEGER NOT NULL,
        alignment_version       INTEGER NOT NULL,
        created_at_ms           INTEGER NOT NULL,
        -- 1 when this refinement's LCS fidelity engaged a cost cap (member-count sample or the
        -- per-pair length proxy). Persisted so a warm cache hit can still report `metrics_sampled`
        -- for the long-sequence dimension. Present in the V029 DDL for fresh DBs; existing-V029
        -- indexes get it via the V030 migration (apply_clone_refinements_lcs_sampled).
        lcs_sampled             INTEGER NOT NULL DEFAULT 0
    ) STRICT;
";

/// V029 (#215): create the clone-detection substrate tables.
///
/// `lcs_sampled` is present in the V029 CREATE TABLE DDL so fresh DBs get it here. Existing
/// indexes recorded at V029 before the column landed are healed by V030
/// (`apply_clone_refinements_lcs_sampled`) — an already-applied migration's apply fn is never
/// re-invoked on an existing DB, so this function cannot add the column retroactively.
///
/// Population gap: V029 only CREATEs the clone tables. Their rows (`symbol_fingerprints` /
/// `symbol_token_postings` / `clone_token_df`) populate as files are (re)indexed — there is no
/// backfill here. An existing index migrated forward therefore has EMPTY clone tables until a
/// `rag-rat index --full`. Backfilling at migration time is intentionally NOT done: it would
/// require parsing the entire repo inside a migration.
pub fn apply_clone_fingerprint_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(CLONE_FINGERPRINT_DDL)?;
    Ok(())
}

/// V034: the precomputed clone-edge graph, so `find_clones` reads a persisted graph instead of
/// recomputing the super-linear SourcererCC candidate pairs every query (it does not finish in 240s
/// on a 118k-function index). Computed at θ = `CLONE_PRECOMPUTE_THETA` (0.7).
///
/// Generation-staged: the resumable background recompute writes a new `build_generation`; reads
/// serve the latest `Complete` generation (the `clone_graph_live_generation` meta key); the pointer
/// flips atomically on completion so a half-built generation is never served. GC of a superseded
/// generation CASCADEs its edges — `clone_graph_generations` is DURABLE precompute metadata, not a
/// `REINDEX_VOLATILE_PARENT`, so that CASCADE FK is allowed.
///
/// CONTENT-ANCHORED endpoints (the #248 bug-class rule, enforced by
/// `no_table_has_a_reindex_cascading_fk_to_a_volatile_parent`): this is durable output that MUST
/// survive reindex, so it carries NO `ON DELETE CASCADE` FK to `symbols` (a
/// `REINDEX_VOLATILE_PARENT` whose ids are reassigned on reindex — keying on `symbol_id` is the
/// exact #248 bug that wiped `edge_oracle` verdicts). Each endpoint is the reindex-stable `(path,
/// start_byte)` of a symbol plus the `file_sha` (`files.sha256`) at compute time — the same
/// content-key/staleness pattern as `edge_oracle`. Reads resolve an endpoint by joining live
/// `symbols`/`files` on `(path, start_byte)` AND `files.sha256 = *_file_sha`; a deleted or edited
/// endpoint simply does not resolve, so a dangling/stale edge is dropped at read (never a ghost
/// member).
///
/// `overlap` + both `token_len`s are the exact `verified_clone` gate inputs, so any query θ ≥ 0.7
/// reproduces `overlap >= ceil(θ * max_len)` precisely by filtering stored rows. Struct-hash exact
/// pairs carry `similarity = 1.0` so they survive every θ.
///
/// Population gap (as with the V029 clone tables): this migration only CREATEs the tables. They
/// populate when a precompute pass runs (watcher maintenance / `rag-rat clones --precompute`);
/// there is no backfill at migration time. Until then `find_clones` uses its live path unchanged.
/// The sub-block inverted index the resumable build streams against is rebuilt in RAM from
/// `symbol_fingerprints.token_bag` each pass (cheap relative to pair emission); a PERSISTED
/// postings table is deferred to the incremental-maintenance follow-up, where it would itself be
/// content-anchored.
pub(crate) const CLONE_GRAPH_DDL: &str = "
    CREATE TABLE IF NOT EXISTS clone_graph_generations(
        generation         INTEGER PRIMARY KEY,
        status             TEXT    NOT NULL CHECK (status IN ('Building', 'Complete')),
        theta_floor        REAL    NOT NULL,
        normalizer_kind    TEXT    NOT NULL,            -- baseline
        normalizer_version INTEGER NOT NULL,            -- NORM_VERSION at build
        source_revision    TEXT    NOT NULL,            -- content_revision() this generation \
                                          builds toward
        cursor_symbol_id   INTEGER NOT NULL DEFAULT 0,  -- build-local resume point (last \
                                          symbol_id emitted)
        edges_written      INTEGER NOT NULL DEFAULT 0,
        started_at_ms      INTEGER NOT NULL,
        finished_at_ms     INTEGER
    ) STRICT;
    CREATE TABLE IF NOT EXISTS clone_edges(
        build_generation INTEGER NOT NULL REFERENCES clone_graph_generations(generation) ON DELETE \
                                          CASCADE,
        -- Content-anchored endpoints: NO symbol_id FK (#248 rule). Canonical a < b by (path, \
                                          start_byte).
        a_path           TEXT    NOT NULL,
        a_start_byte     INTEGER NOT NULL,
        a_file_sha       TEXT    NOT NULL,              -- files.sha256 at compute; read-time \
                                          staleness filter
        b_path           TEXT    NOT NULL,
        b_start_byte     INTEGER NOT NULL,
        b_file_sha       TEXT    NOT NULL,
        overlap          INTEGER NOT NULL,              -- Σ min(freq) = verified_clone overlap
        a_token_len      INTEGER NOT NULL,
        b_token_len      INTEGER NOT NULL,
        similarity       REAL    NOT NULL,              -- overlap/max_len; 1.0 for \
                                          struct-hash-exact pairs
        edge_source      TEXT    NOT NULL,              -- 'struct_hash' | 'sub_block'
        PRIMARY KEY (build_generation, a_path, a_start_byte, b_path, b_start_byte)
    ) STRICT;
    CREATE INDEX IF NOT EXISTS idx_clone_edges_b
        ON clone_edges(build_generation, b_path, b_start_byte);
";

/// V034: create the precomputed clone-graph tables (see [`CLONE_GRAPH_DDL`]).
pub fn apply_clone_graph_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(CLONE_GRAPH_DDL)?;
    Ok(())
}

/// V037 (#296): PERSIST the sub-block postings the write-time clone check reads, so that check
/// SCALES past the 40k-function guard. `clones_of_text` (the write-time hook engine) today rebuilds
/// the whole `(sub_block_token -> symbol)` inverted index in RAM on every call (O(functions)), so
/// the hook no-ops above `MAX_CLONE_CHECK_FUNCTIONS`; persisting the postings lets it do a bounded
/// indexed lookup per new function instead. This is the follow-up the V034 doc named: the slot was
/// deliberately DEFERRED there (see [`CLONE_GRAPH_DDL`]) and is filled here.
///
/// CONTENT-ANCHORED endpoints (the #248 bug-class rule, enforced by
/// `no_table_has_a_reindex_cascading_fk_to_a_volatile_parent`): a posting anchors to the
/// reindex-stable `(path, start_byte)` of a symbol plus the `file_sha` (`files.sha256`) at compute
/// time — NEVER a `symbol_id` FK (`symbols` is a `REINDEX_VOLATILE_PARENT` whose ids are reassigned
/// on reindex; keying on `symbol_id` is the exact #248 bug that wiped `edge_oracle` verdicts). The
/// read resolves an anchor by joining live `symbols`/`files` on `(path, start_byte)` and drops any
/// row whose `file_sha` no longer matches the last-indexed `files.sha256`, so a stale posting is
/// silently ignored rather than matched against changed content. The ONLY FK is the `ON DELETE
/// CASCADE` to the DURABLE `clone_graph_generations` (precompute metadata, not a
/// `REINDEX_VOLATILE_PARENT`, so that CASCADE is allowed): postings live and die with their build
/// generation, and a superseded generation's postings are GC'd by that cascade — the same
/// generation-staged lifecycle `clone_edges` uses, no independent freshness key.
///
/// The write-time lookup is `WHERE build_generation = ? AND token_hash IN (…)`, which
/// `idx_clone_subblock_postings_token` covers directly. This migration only CREATEs the empty table
/// (population + the read-path switch land in later phases of #296); until then the write-time
/// check keeps its RAM-index fallback unchanged.
pub(crate) const CLONE_SUBBLOCK_POSTINGS_DDL: &str = "
    CREATE TABLE IF NOT EXISTS clone_subblock_postings(
        build_generation INTEGER NOT NULL REFERENCES clone_graph_generations(generation) ON DELETE \
                                                      CASCADE,
        token_hash       INTEGER NOT NULL,
        -- Content anchor (reindex-stable), NOT symbol_id (the #248 rule).
        path             TEXT    NOT NULL,
        start_byte       INTEGER NOT NULL,
        file_sha         TEXT    NOT NULL,              -- files.sha256 at compute; read-time \
                                                      staleness key
        PRIMARY KEY (build_generation, token_hash, path, start_byte)
    ) STRICT;
    CREATE INDEX IF NOT EXISTS idx_clone_subblock_postings_token
        ON clone_subblock_postings(build_generation, token_hash);
";

/// V037 (#296): create the persisted sub-block postings table (see
/// [`CLONE_SUBBLOCK_POSTINGS_DDL`]) and add `clone_graph_generations.postings_written` — the
/// upgrade-repopulation gate (review R2). A clone-graph generation built before this feature has
/// `postings_written = 0`, which the (phase-2) precompute reads as "not postings-complete" and uses
/// to force one rebuild pass that fills the postings, instead of leaving an upgraded DB with an
/// empty table forever. Idempotent: `CREATE TABLE IF NOT EXISTS` + `add_column_if_missing`.
pub(crate) fn apply_clone_subblock_postings_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(CLONE_SUBBLOCK_POSTINGS_DDL)?;
    add_column_if_missing(
        conn,
        "clone_graph_generations",
        "postings_written",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(())
}
