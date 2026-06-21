mod baseline;
mod migrations;
pub(crate) use baseline::*;
pub(crate) use migrations::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub const LATEST_SCHEMA_VERSION: u32 = 31;

/// Every oracle-DERIVED persisted table — the outputs an `oracle run` writes that must OUTLIVE a
/// reindex.
///
/// INVARIANT (load-bearing, #248): oracle-derived outputs MUST survive reindex. They achieve this
/// by being **content-keyed with NO reindex-cascading FK** to a reindex-volatile parent
/// ([`REINDEX_VOLATILE_PARENTS`]); their reads JOIN the live parents by that content key, so a
/// dangling row (whose parent was rewritten) simply never resolves rather than being CASCADE-wiped.
/// This is the model `logical_symbol_monikers` shipped (#70) and `edge_oracle` was retrofitted to
/// (#248) after its `FOREIGN KEY(edge_id) REFERENCES edges_data(id) ON DELETE CASCADE` silently
/// wiped every verdict on the first reindex.
///
/// The ENFORCING structural trip-wire `no_table_has_a_reindex_cascading_fk_to_a_volatile_parent`
/// scans EVERY table in `sqlite_master` (not this list) and asserts none carries an `ON DELETE
/// CASCADE`/`RESTRICT` FK to a volatile parent except the explicit [`CASCADE_FK_ALLOWLIST`] — the
/// exact check that would have caught the original `edge_oracle` FK, and which catches a future
/// oracle/durable table automatically even if it forgets to opt into any list. This const remains
/// the canonical DECLARATION of which outputs must survive reindex — the same trip-wire asserts
/// each listed table exists, and the lifecycle guard
/// `oracle_outputs_survive_full_and_incremental_reindex` exercises their survival behaviorally
/// across both reindex shapes.
// Consumed only by the reindex trip-wires (#[cfg(test)]), so the non-test lib build sees no reader;
// it is intentionally a durable schema-invariant declaration.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const ORACLE_PERSISTED_TABLES: &[&str] =
    &["edge_oracle", "logical_symbol_monikers", "oracle_runs"];

/// The parent tables a reindex REWRITES (full rebuild and/or per-file `remove_file_in_scope`), so
/// an `ON DELETE CASCADE`/`RESTRICT` FK from an oracle-derived table to one of these wipes the
/// oracle output on every reindex — the #248 bug class. The trip-wire forbids exactly such an FK.
/// `files` is rowid-keyed and rewritten per file; `edges_data` / `symbols` / `logical_symbols` are
/// all rebuilt (DELETE-all + reinsert) on a full reindex.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const REINDEX_VOLATILE_PARENTS: &[&str] =
    &["edges_data", "symbols", "logical_symbols", "files"];

/// The `(child_table, volatile_parent)` pairs that LEGITIMATELY carry an `ON DELETE
/// CASCADE`/`RESTRICT` FK to a reindex-volatile parent ([`REINDEX_VOLATILE_PARENTS`]) — every one a
/// table that is *rebuilt with its parent* on every reindex and holds NO oracle/durable state, so
/// the cascade is the desired freshness behavior (the child must die with the parent row that
/// produced it), not the #248 data-loss bug.
///
/// INVARIANT (#248): this is the EXPLICIT opt-out the enforcing trip-wire
/// [`no_table_has_a_reindex_cascading_fk_to_a_volatile_parent`] consults. The trip-wire scans EVERY
/// table in `sqlite_master` (not a hand-maintained list), so a NEW table that adds a cascading FK
/// to a volatile parent FAILS the test automatically unless it is added here WITH a reason. Never
/// allowlist a table that holds oracle/durable state — that re-creates the #248 bug; re-anchor such
/// a table on a content key + drop the FK instead.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const CASCADE_FK_ALLOWLIST: &[(&str, &str)] = &[
    // `chunks` are per-file text/embedding inputs, re-chunked from scratch when a file is
    // reindexed.
    ("chunks", "files"),
    // `edges_data` is the heuristic graph itself, rebuilt per-file (it IS a volatile parent).
    ("edges_data", "files"),
    // `symbols` are rebuilt per-file (AUTOINCREMENT rowids re-mint on reindex).
    ("symbols", "files"),
    // Logical-symbol grouping is rebuilt wholesale from the live symbols on every pass.
    ("logical_symbol_members", "symbols"),
    ("logical_symbol_members", "logical_symbols"),
    // Per-symbol derived facts, rebuilt with the symbols they describe.
    ("symbol_facts", "symbols"),
    // Clone-detection fingerprints + their inverted-index postings, rebuilt with the symbols.
    ("symbol_fingerprints", "symbols"),
    ("symbol_token_postings", "symbols"),
];

const DIRTY_MIGRATION_ID: &str = "__dirty__";
const MIGRATION_001_ID: &str = "001_sqlite_storage_baseline";
const MIGRATION_001_CHECKSUM: &str = "sha256:rag-rat-sqlite-baseline-v1";
const MIGRATION_001_DESCRIPTION: &str =
    "SQLite storage baseline with FTS, tree-sitter graph edges, git/GitHub, and local AI metadata";
const MIGRATION_002_ID: &str = "002_embedding_vector_metadata";
const MIGRATION_002_CHECKSUM: &str = "sha256:rag-rat-embedding-vector-metadata-v2";
const MIGRATION_002_DESCRIPTION: &str =
    "Add embedding model dimension metadata and per-vector dimensions for hybrid vector search";
const MIGRATION_003_ID: &str = "003_derived_artifact_reconcile_metadata";
const MIGRATION_003_CHECKSUM: &str = "sha256:rag-rat-derived-artifact-reconcile-metadata-v3";
const MIGRATION_003_DESCRIPTION: &str = "Add model version, retry metadata, summaries, and \
                                         reconcile meta for diff-based derived artifact \
                                         reconciliation";
const MIGRATION_004_ID: &str = "004_edge_source_target_spans";
const MIGRATION_004_CHECKSUM: &str = "sha256:rag-rat-edge-source-target-spans-v4";
const MIGRATION_004_DESCRIPTION: &str =
    "Add exact source call-site spans and resolved target line spans to graph edges";
const MIGRATION_005_ID: &str = "005_edge_evidence_and_resolution";
const MIGRATION_005_CHECKSUM: &str = "sha256:rag-rat-edge-evidence-resolution-v5";
const MIGRATION_005_DESCRIPTION: &str =
    "Add raw graph edge evidence, receiver hints, qualified targets, and resolution reasons";
const MIGRATION_006_ID: &str = "006_embedding_policy_and_input_hash";
const MIGRATION_006_CHECKSUM: &str = "sha256:rag-rat-embedding-policy-input-hash-v6";
const MIGRATION_006_DESCRIPTION: &str = "Add embedding eligibility policy, priority, bounded \
                                         input hash, and reconcile throughput metadata";
const MIGRATION_007_ID: &str = "007_logical_symbol_groups";
const MIGRATION_007_CHECKSUM: &str = "sha256:rag-rat-logical-symbol-groups-v7";
const MIGRATION_007_DESCRIPTION: &str =
    "Add logical symbol groups for cfg variants and duplicate definitions";
const MIGRATION_008_ID: &str = "008_commit_addressable_worktrees";
const MIGRATION_008_CHECKSUM: &str = "sha256:rag-rat-commit-addressable-worktrees-v8";
const MIGRATION_008_DESCRIPTION: &str =
    "Add commit_sha and worktree_id to files table for multi-worktree / multi-branch support";
const MIGRATION_009_ID: &str = "009_github_ref_sync_state";
const MIGRATION_009_CHECKSUM: &str = "sha256:rag-rat-github-ref-sync-state-v9";
const MIGRATION_009_DESCRIPTION: &str =
    "Add per-GitHub-ref sync state for resumable papertrail cache updates";
const MIGRATION_010_ID: &str = "010_symbol_facts";
const MIGRATION_010_CHECKSUM: &str = "sha256:rag-rat-symbol-facts-v10";
const MIGRATION_010_DESCRIPTION: &str =
    "Add normalized symbol facts for parsed language metadata such as Rust attributes";
const MIGRATION_011_ID: &str = "011_repo_memories";
const MIGRATION_011_CHECKSUM: &str = "sha256:rag-rat-repo-memories-v11";
const MIGRATION_011_DESCRIPTION: &str =
    "Add source-anchored repo memories bound to symbols, chunks, paths, and papertrail refs";
const MIGRATION_012_ID: &str = "012_repo_memory_call_paths";
const MIGRATION_012_CHECKSUM: &str = "sha256:rag-rat-repo-memory-call-paths-v12";
const MIGRATION_012_DESCRIPTION: &str =
    "Add edge and call-path memory bindings for graph traversal surfacing";
const MIGRATION_013_ID: &str = "013_graph_file_lookup_indexes";
const MIGRATION_013_CHECKSUM: &str = "sha256:rag-rat-graph-file-lookup-indexes-v13";
const MIGRATION_013_DESCRIPTION: &str =
    "Add graph file lookup indexes for ownership clustering and file-level graph summaries";
const MIGRATION_014_ID: &str = "014_repo_memory_binding_signals";
const MIGRATION_014_CHECKSUM: &str = "sha256:rag-rat-repo-memory-binding-signals-v14";
const MIGRATION_014_DESCRIPTION: &str =
    "Add symbol_kind + signature_hash to repo_memory_bindings for durable cross-file relocation";
const MIGRATION_015_ID: &str = "015_repo_memory_call_path_edges";
const MIGRATION_015_CHECKSUM: &str = "sha256:rag-rat-repo-memory-call-path-edges-v15";
const MIGRATION_015_DESCRIPTION: &str =
    "Add ordered edge fingerprints behind server-derived call-path hashes for validation";
const MIGRATION_016_ID: &str = "016_symbol_line_spans";
const MIGRATION_016_CHECKSUM: &str = "sha256:rag-rat-symbol-line-spans-v16";
const MIGRATION_016_DESCRIPTION: &str = "Store start_line/end_line on symbols so readers skip the \
                                         per-symbol chunk-containment subqueries";
const MIGRATION_017_ID: &str = "017_edge_callee_byte_range";
const MIGRATION_017_CHECKSUM: &str = "sha256:rag-rat-edge-callee-byte-range-v17";
const MIGRATION_017_DESCRIPTION: &str =
    "Add callee identifier byte range to edges for the SCIP occurrence join (#61 prerequisite)";
const MIGRATION_018_ID: &str = "018_scip_oracle_tables";
const MIGRATION_018_CHECKSUM: &str = "sha256:rag-rat-scip-oracle-tables-v18";
const MIGRATION_018_DESCRIPTION: &str =
    "Add oracle_runs + edge_oracle side tables for SCIP compiler-grade edge resolution (#68)";
const MIGRATION_019_ID: &str = "019_scip_moniker_anchors";
const MIGRATION_019_CHECKSUM: &str = "sha256:rag-rat-scip-moniker-anchors-v19";
const MIGRATION_019_DESCRIPTION: &str = "Add logical_symbol_monikers + moniker provenance and \
                                         relocation reason on repo memory bindings (#70)";
const MIGRATION_020_ID: &str = "020_edge_string_interning";
const MIGRATION_020_CHECKSUM: &str = "sha256:rag-rat-edge-string-interning-v20";
const MIGRATION_020_DESCRIPTION: &str = "Normalize repeated edge strings into the name_strings \
                                         dictionary behind the edges compatibility view (#79)";
const MIGRATION_021_ID: &str = "021_symbol_scope_path";
const MIGRATION_021_CHECKSUM: &str = "sha256:rag-rat-symbol-scope-path-v21";
const MIGRATION_021_DESCRIPTION: &str =
    "Add symbols.scope_path (semantic enclosing-scope path) for scope-aware edge resolution (#61)";
const MIGRATION_022_ID: &str = "022_per_package_import_scope";
const MIGRATION_022_CHECKSUM: &str = "sha256:rag-rat-per-package-import-scope-v22";
const MIGRATION_022_DESCRIPTION: &str = "Add packages table + dedicated edge import-scope columns \
                                         for per-package, module-aware import resolution (#61)";
const MIGRATION_023_ID: &str = "023_dispatch_edge_facts_view_exclusion";
const MIGRATION_023_CHECKSUM: &str = "sha256:rag-rat-dispatch-edge-facts-view-exclusion-v23";
const MIGRATION_023_DESCRIPTION: &str = "Recreate the edges compatibility view to exclude \
                                         internal dispatch FACT rows from query-layer reads (#200)";
const MIGRATION_024_ID: &str = "024_files_has_test_code";
const MIGRATION_024_CHECKSUM: &str = "sha256:rag-rat-files-has-test-code-v24";
const MIGRATION_024_DESCRIPTION: &str = "Add files.has_test_code flag (precomputed test-marker \
                                         detection) so impact_surface avoids a chunks.text scan \
                                         (#77)";
const MIGRATION_025_ID: &str = "025_chunk_text_compression_tables";
const MIGRATION_025_CHECKSUM: &str = "sha256:rag-rat-chunk-text-compression-tables-v25";
const MIGRATION_025_DESCRIPTION: &str = "Add chunk_text (zstd blob) + chunk_text_dict (shared \
                                         dictionary) tables for compressed chunk text (#77)";
const MIGRATION_026_ID: &str = "026_contentless_chunk_fts";
const MIGRATION_026_CHECKSUM: &str = "sha256:rag-rat-contentless-chunk-fts-v26";
const MIGRATION_026_DESCRIPTION: &str = "Recreate chunk_fts as a contentless FTS5 index and \
                                         repopulate it, so chunks.text can be dropped (#77 Phase \
                                         2)";
const MIGRATION_027_ID: &str = "027_drop_chunks_text";
const MIGRATION_027_CHECKSUM: &str = "sha256:rag-rat-drop-chunks-text-v27";
const MIGRATION_027_DESCRIPTION: &str = "Build the compressed chunk_text store from chunks.text, \
                                         then drop the chunks.text column (#77 Phase 2)";
const MIGRATION_028_ID: &str = "028_intern_symbol_qualified_names";
const MIGRATION_028_CHECKSUM: &str = "sha256:rag-rat-intern-symbol-qualified-names-v28";
const MIGRATION_028_DESCRIPTION: &str = "Intern symbols/logical_symbols qualified_name into the \
                                         shared name_strings pool, then drop the columns (#224)";
const MIGRATION_029_ID: &str = "029_clone_fingerprint_tables";
const MIGRATION_029_CHECKSUM: &str = "sha256:rag-rat-clone-fingerprint-tables-v29";
const MIGRATION_029_DESCRIPTION: &str = "Add symbol_fingerprints + symbol_token_postings + \
                                         clone_token_df + clone_refinements for clone detection \
                                         (#215)";
const MIGRATION_030_ID: &str = "030_clone_refinements_lcs_sampled";
const MIGRATION_030_CHECKSUM: &str = "sha256:rag-rat-clone-refinements-lcs-sampled-v30";
const MIGRATION_030_DESCRIPTION: &str =
    "Add clone_refinements.lcs_sampled (additive; heals indexes already at V029)";
const MIGRATION_031_ID: &str = "031_edge_oracle_content_anchor";
const MIGRATION_031_CHECKSUM: &str = "sha256:rag-rat-edge-oracle-content-anchor-v31";
const MIGRATION_031_DESCRIPTION: &str = "Rebuild edge_oracle content-anchored (drop edges_data FK \
                                         + edge_id PK) so verdicts survive reindex (#248)";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SchemaState {
    Missing,
    Compatible,
    Older,
    Newer,
    Dirty,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppliedMigration {
    pub id: String,
    pub applied_at_ms: i64,
    pub checksum: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaStatus {
    pub state: SchemaState,
    pub current_version: u32,
    pub latest_version: u32,
    pub migrations: Vec<AppliedMigration>,
    pub message: String,
}

/// Provision the baseline (001) idempotently: the schema_version ledger, then `apply_baseline`
/// (all `CREATE … IF NOT EXISTS`) wrapped in the dirty marker so a crash mid-baseline is
/// detectable, then record 001. Shared by [`apply`] (fresh DB) and [`migrate_forward`] (existing DB
/// behind by N). LOAD-BEARING for forward-only: a pre-interning (≤v19) DB lacks shared tables like
/// `name_strings`/`edges_data` that later migrations INSERT into, so the baseline must run BEFORE
/// any forward step replays — exactly what the old full `apply` did before the ladder.
fn provision_baseline(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_version(
            id TEXT PRIMARY KEY,
            applied_at_ms INTEGER NOT NULL,
            checksum TEXT NOT NULL,
            description TEXT NOT NULL
        );
        ",
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO schema_version(id, applied_at_ms, checksum, description)
         VALUES (?1, ?2, ?3, ?4)",
        params![DIRTY_MIGRATION_ID, now_ms(), "", "partial migration in progress"],
    )?;
    // The string pool was `edge_strings` before the name-pool merge (#224, the V028 bump); it now
    // also holds symbol qualified-names, so the current schema names it `name_strings`. On a
    // pre-merge DB the POPULATED table is still `edge_strings` — rename it into place HERE, before
    // `apply_baseline`'s `CREATE TABLE IF NOT EXISTS name_strings` (and the `ensure_edges_view` it
    // calls), so we adopt the real table instead of creating an empty one beside it (which would
    // orphan every edge's interned names). Runs before any migration replay, so V023's
    // `ensure_edges_view` then sees `name_strings`. Fresh / already-merged DBs have no
    // `edge_strings` → no-op. A pre-merge DB is `Older` (< V028), so it reaches this migrate path;
    // a `Compatible` open skips migration and is already `name_strings`.
    let pre_merge_pool: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'edge_strings'",
        [],
        |row| row.get(0),
    )?;
    let merged_pool: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'name_strings'",
        [],
        |row| row.get(0),
    )?;
    if pre_merge_pool > 0 && merged_pool == 0 {
        conn.execute_batch("ALTER TABLE edge_strings RENAME TO name_strings;")?;
    }
    let result = apply_baseline(conn);
    if let Err(err) = result {
        let _ = conn.execute("UPDATE schema_version SET description = ?2 WHERE id = ?1", params![
            DIRTY_MIGRATION_ID,
            format!("partial migration failed: {err}")
        ]);
        return Err(err);
    }
    conn.execute("DELETE FROM schema_version WHERE id = ?1", [DIRTY_MIGRATION_ID])?;
    record_migration(conn, MIGRATION_001_ID, MIGRATION_001_CHECKSUM, MIGRATION_001_DESCRIPTION)
}

pub fn apply(conn: &Connection) -> rusqlite::Result<()> {
    provision_baseline(conn)?;
    // Every additive migration in order. A fresh DB runs them all; data backfills on empty tables
    // are no-ops. (An EXISTING DB takes the forward-only path via `migrate_forward`, not this.)
    for step in ADDITIVE_MIGRATIONS {
        (step.apply)(conn)?;
        record_migration(conn, step.id, step.checksum, step.description)?;
    }
    Ok(())
}

/// One additive migration layered on the baseline (001): its identity + the function that applies
/// it. Single source of truth for both `apply` (fresh DB, runs all) and `migrate_forward` (existing
/// DB, runs only the unapplied ones).
struct Migration {
    id: &'static str,
    checksum: &'static str,
    description: &'static str,
    apply: fn(&Connection) -> rusqlite::Result<()>,
}

const ADDITIVE_MIGRATIONS: &[Migration] = &[
    Migration {
        id: MIGRATION_002_ID,
        checksum: MIGRATION_002_CHECKSUM,
        description: MIGRATION_002_DESCRIPTION,
        apply: apply_embedding_vector_metadata,
    },
    Migration {
        id: MIGRATION_003_ID,
        checksum: MIGRATION_003_CHECKSUM,
        description: MIGRATION_003_DESCRIPTION,
        apply: apply_derived_artifact_reconcile_metadata,
    },
    Migration {
        id: MIGRATION_004_ID,
        checksum: MIGRATION_004_CHECKSUM,
        description: MIGRATION_004_DESCRIPTION,
        apply: apply_edge_source_target_spans,
    },
    Migration {
        id: MIGRATION_005_ID,
        checksum: MIGRATION_005_CHECKSUM,
        description: MIGRATION_005_DESCRIPTION,
        apply: apply_edge_evidence_and_resolution,
    },
    Migration {
        id: MIGRATION_006_ID,
        checksum: MIGRATION_006_CHECKSUM,
        description: MIGRATION_006_DESCRIPTION,
        apply: apply_embedding_policy_and_input_hash,
    },
    Migration {
        id: MIGRATION_007_ID,
        checksum: MIGRATION_007_CHECKSUM,
        description: MIGRATION_007_DESCRIPTION,
        apply: apply_logical_symbol_groups,
    },
    Migration {
        id: MIGRATION_008_ID,
        checksum: MIGRATION_008_CHECKSUM,
        description: MIGRATION_008_DESCRIPTION,
        apply: apply_commit_addressable_worktrees,
    },
    Migration {
        id: MIGRATION_009_ID,
        checksum: MIGRATION_009_CHECKSUM,
        description: MIGRATION_009_DESCRIPTION,
        apply: apply_github_ref_sync,
    },
    Migration {
        id: MIGRATION_010_ID,
        checksum: MIGRATION_010_CHECKSUM,
        description: MIGRATION_010_DESCRIPTION,
        apply: apply_symbol_facts,
    },
    Migration {
        id: MIGRATION_011_ID,
        checksum: MIGRATION_011_CHECKSUM,
        description: MIGRATION_011_DESCRIPTION,
        apply: apply_repo_memories,
    },
    Migration {
        id: MIGRATION_012_ID,
        checksum: MIGRATION_012_CHECKSUM,
        description: MIGRATION_012_DESCRIPTION,
        apply: apply_repo_memory_call_paths,
    },
    Migration {
        id: MIGRATION_013_ID,
        checksum: MIGRATION_013_CHECKSUM,
        description: MIGRATION_013_DESCRIPTION,
        apply: apply_graph_file_lookup_indexes,
    },
    Migration {
        id: MIGRATION_014_ID,
        checksum: MIGRATION_014_CHECKSUM,
        description: MIGRATION_014_DESCRIPTION,
        apply: apply_memory_binding_signals,
    },
    Migration {
        id: MIGRATION_015_ID,
        checksum: MIGRATION_015_CHECKSUM,
        description: MIGRATION_015_DESCRIPTION,
        apply: apply_repo_memory_call_path_edges,
    },
    Migration {
        id: MIGRATION_016_ID,
        checksum: MIGRATION_016_CHECKSUM,
        description: MIGRATION_016_DESCRIPTION,
        apply: apply_symbol_line_spans,
    },
    Migration {
        id: MIGRATION_017_ID,
        checksum: MIGRATION_017_CHECKSUM,
        description: MIGRATION_017_DESCRIPTION,
        apply: apply_edge_callee_byte_range,
    },
    Migration {
        id: MIGRATION_018_ID,
        checksum: MIGRATION_018_CHECKSUM,
        description: MIGRATION_018_DESCRIPTION,
        apply: apply_oracle_tables,
    },
    Migration {
        id: MIGRATION_019_ID,
        checksum: MIGRATION_019_CHECKSUM,
        description: MIGRATION_019_DESCRIPTION,
        apply: apply_scip_moniker_anchors,
    },
    Migration {
        id: MIGRATION_020_ID,
        checksum: MIGRATION_020_CHECKSUM,
        description: MIGRATION_020_DESCRIPTION,
        apply: apply_edge_string_interning,
    },
    Migration {
        id: MIGRATION_021_ID,
        checksum: MIGRATION_021_CHECKSUM,
        description: MIGRATION_021_DESCRIPTION,
        apply: apply_symbol_scope_path,
    },
    Migration {
        id: MIGRATION_022_ID,
        checksum: MIGRATION_022_CHECKSUM,
        description: MIGRATION_022_DESCRIPTION,
        apply: apply_per_package_import_scope,
    },
    Migration {
        id: MIGRATION_023_ID,
        checksum: MIGRATION_023_CHECKSUM,
        description: MIGRATION_023_DESCRIPTION,
        apply: apply_dispatch_edge_facts_view_exclusion,
    },
    Migration {
        id: MIGRATION_024_ID,
        checksum: MIGRATION_024_CHECKSUM,
        description: MIGRATION_024_DESCRIPTION,
        apply: apply_files_has_test_code,
    },
    Migration {
        id: MIGRATION_025_ID,
        checksum: MIGRATION_025_CHECKSUM,
        description: MIGRATION_025_DESCRIPTION,
        apply: apply_chunk_text_compression_tables,
    },
    Migration {
        id: MIGRATION_026_ID,
        checksum: MIGRATION_026_CHECKSUM,
        description: MIGRATION_026_DESCRIPTION,
        apply: apply_contentless_chunk_fts,
    },
    Migration {
        id: MIGRATION_027_ID,
        checksum: MIGRATION_027_CHECKSUM,
        description: MIGRATION_027_DESCRIPTION,
        apply: apply_drop_chunks_text,
    },
    Migration {
        id: MIGRATION_028_ID,
        checksum: MIGRATION_028_CHECKSUM,
        description: MIGRATION_028_DESCRIPTION,
        apply: apply_intern_symbol_qualified_names,
    },
    Migration {
        id: MIGRATION_029_ID,
        checksum: MIGRATION_029_CHECKSUM,
        description: MIGRATION_029_DESCRIPTION,
        apply: apply_clone_fingerprint_tables,
    },
    Migration {
        id: MIGRATION_030_ID,
        checksum: MIGRATION_030_CHECKSUM,
        description: MIGRATION_030_DESCRIPTION,
        apply: apply_clone_refinements_lcs_sampled,
    },
    Migration {
        id: MIGRATION_031_ID,
        checksum: MIGRATION_031_CHECKSUM,
        description: MIGRATION_031_DESCRIPTION,
        apply: apply_edge_oracle_content_anchor,
    },
];

/// Apply ONLY the additive migrations not already recorded, in order — the forward-only path for an
/// existing index that lags this binary. It first provisions the baseline idempotently (so a
/// ≤v19 DB that predates a shared table like `name_strings`/`edges_data` has it before a later
/// migration INSERTs into it), then replays just the unapplied steps. Unlike [`apply`], it never
/// re-runs an already-applied migration, so a data backfill like 005's resolution rewrite (an
/// unconditional UPDATE) cannot clobber current values on a routine open. The caller guarantees a
/// versioned ledger exists; a ledger-less legacy DB is refused upstream, not force-migrated.
pub fn migrate_forward(conn: &Connection) -> anyhow::Result<()> {
    provision_baseline(conn)?;
    let applied: std::collections::HashSet<String> =
        applied_migrations(conn)?.into_iter().map(|migration| migration.id).collect();
    for step in ADDITIVE_MIGRATIONS {
        if !applied.contains(step.id) {
            (step.apply)(conn)?;
            record_migration(conn, step.id, step.checksum, step.description)?;
        }
    }
    Ok(())
}

pub fn status(conn: &Connection) -> anyhow::Result<SchemaStatus> {
    if !table_exists(conn, "schema_version")? {
        let has_legacy_tables = table_exists(conn, "files")? || table_exists(conn, "chunks")?;
        return Ok(if has_legacy_tables {
            SchemaStatus {
                state: SchemaState::Older,
                current_version: 0,
                latest_version: LATEST_SCHEMA_VERSION,
                migrations: Vec::new(),
                message: "legacy index schema has no version ledger; rebuild the derived index \
                          with `rag-rat index --full`"
                    .to_string(),
            }
        } else {
            SchemaStatus {
                state: SchemaState::Missing,
                current_version: 0,
                latest_version: LATEST_SCHEMA_VERSION,
                migrations: Vec::new(),
                message: "index schema is not initialized; build the derived index with `rag-rat \
                          index` or `rag-rat index --full`"
                    .to_string(),
            }
        });
    }

    let migrations = applied_migrations(conn)?;
    if migrations.iter().any(|migration| migration.id == DIRTY_MIGRATION_ID) {
        return Ok(SchemaStatus {
            state: SchemaState::Dirty,
            current_version: known_version(&migrations),
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            message: "dirty or partial schema migration detected; rebuild the derived index with \
                      `rag-rat index --full`"
                .to_string(),
        });
    }
    if migrations.iter().any(migration_checksum_mismatch) {
        return Ok(SchemaStatus {
            state: SchemaState::Dirty,
            current_version: known_version(&migrations),
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            message: "schema migration checksum mismatch; refusing to open, rebuild the derived \
                      index with `rag-rat index --full`"
                .to_string(),
        });
    }
    if migrations.iter().any(|migration| !known_migration(&migration.id)) {
        return Ok(SchemaStatus {
            state: SchemaState::Newer,
            current_version: known_version(&migrations),
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            message: "index schema was created by a newer rag-rat; refusing to open".to_string(),
        });
    }
    let current_version = known_version(&migrations);
    if current_version < LATEST_SCHEMA_VERSION {
        return Ok(SchemaStatus {
            state: SchemaState::Older,
            current_version,
            latest_version: LATEST_SCHEMA_VERSION,
            migrations,
            message: "index schema is older than this rag-rat; it migrates forward automatically \
                      on open (or rebuild with `rag-rat index --full`)"
                .to_string(),
        });
    }
    Ok(SchemaStatus {
        state: SchemaState::Compatible,
        current_version,
        latest_version: LATEST_SCHEMA_VERSION,
        migrations,
        message: "schema is compatible".to_string(),
    })
}

/// Make the open-able index schema current, migrating FORWARD automatically when it lags this
/// binary. The index is rag-rat's own derived data, so upgrading the binary must never make a
/// developer hand-run `migrate` (or play devops) to get queries/memory writes working again — an
/// `Older` schema is applied in place here. Migrations are additive and idempotent (the same
/// `apply` the `migrate` command runs on `Older`), so bringing a partly-migrated DB to latest is
/// safe and a no-op for already-present tables/columns.
///
/// Only genuinely unrecoverable states still refuse:
/// - `Newer`: created by a future rag-rat — this binary can't apply migrations it doesn't have, and
///   downgrading derived data isn't safe.
/// - `Dirty`: a partial/crashed migration or checksum mismatch — needs a clean `index --full`.
/// - `Missing`: there is no index at this path yet — nothing to migrate; build one first.
pub fn ensure_compatible_or_migrate(conn: &Connection) -> anyhow::Result<()> {
    let current = status(conn)?;
    match current.state {
        SchemaState::Compatible => Ok(()),
        // Forward migration is automatic — but FORWARD-ONLY: apply just the unapplied migrations,
        // never the whole ladder. Re-running an applied data migration (e.g. 005's unconditional
        // `UPDATE edges SET resolution = …`) would clobber current resolver reasons on a routine
        // open. Forward-only needs a real version ledger to know what's applied; a legacy DB with
        // NO schema_version table (current_version 0) can't be safely advanced — we'd have to
        // re-run those data migrations — so it's refused, not force-migrated. Re-verify Compatible
        // so a half-migration can't slip through as "opened fine".
        SchemaState::Older => {
            if !table_exists(conn, "schema_version")? {
                anyhow::bail!(
                    "legacy index schema has no version ledger and can't be safely migrated \
                     forward; rebuild with `rag-rat index --full`"
                );
            }
            migrate_forward(conn)?;
            let after = status(conn)?;
            if after.state == SchemaState::Compatible {
                Ok(())
            } else {
                anyhow::bail!(
                    "auto-migration left the schema {:?}, not compatible; rebuild the derived \
                     index with `rag-rat index --full`",
                    after.state
                )
            }
        },
        SchemaState::Missing => anyhow::bail!(
            "no index at this path yet; build one with `rag-rat index` or `rag-rat index --full`"
        ),
        SchemaState::Newer | SchemaState::Dirty => anyhow::bail!("{}", current.message),
    }
}
