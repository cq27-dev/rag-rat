mod baseline;
mod migrations;
pub(crate) use baseline::*;
pub(crate) use migrations::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub const LATEST_SCHEMA_VERSION: u32 = 22;
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
const MIGRATION_020_DESCRIPTION: &str = "Normalize repeated edge strings into the edge_strings \
                                         dictionary behind the edges compatibility view (#79)";
const MIGRATION_021_ID: &str = "021_symbol_scope_path";
const MIGRATION_021_CHECKSUM: &str = "sha256:rag-rat-symbol-scope-path-v21";
const MIGRATION_021_DESCRIPTION: &str =
    "Add symbols.scope_path (semantic enclosing-scope path) for scope-aware edge resolution (#61)";
const MIGRATION_022_ID: &str = "022_per_package_import_scope";
const MIGRATION_022_CHECKSUM: &str = "sha256:rag-rat-per-package-import-scope-v22";
const MIGRATION_022_DESCRIPTION: &str = "Add packages table + dedicated edge import-scope columns \
                                         for per-package, module-aware import resolution (#61)";

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

pub fn apply(conn: &Connection) -> rusqlite::Result<()> {
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
    let result = apply_baseline(conn);
    if let Err(err) = result {
        let _ = conn.execute("UPDATE schema_version SET description = ?2 WHERE id = ?1", params![
            DIRTY_MIGRATION_ID,
            format!("partial migration failed: {err}")
        ]);
        return Err(err);
    }
    conn.execute("DELETE FROM schema_version WHERE id = ?1", [DIRTY_MIGRATION_ID])?;
    record_migration(conn, MIGRATION_001_ID, MIGRATION_001_CHECKSUM, MIGRATION_001_DESCRIPTION)?;
    apply_embedding_vector_metadata(conn)?;
    record_migration(conn, MIGRATION_002_ID, MIGRATION_002_CHECKSUM, MIGRATION_002_DESCRIPTION)?;
    apply_derived_artifact_reconcile_metadata(conn)?;
    record_migration(conn, MIGRATION_003_ID, MIGRATION_003_CHECKSUM, MIGRATION_003_DESCRIPTION)?;
    apply_edge_source_target_spans(conn)?;
    record_migration(conn, MIGRATION_004_ID, MIGRATION_004_CHECKSUM, MIGRATION_004_DESCRIPTION)?;
    apply_edge_evidence_and_resolution(conn)?;
    record_migration(conn, MIGRATION_005_ID, MIGRATION_005_CHECKSUM, MIGRATION_005_DESCRIPTION)?;
    apply_embedding_policy_and_input_hash(conn)?;
    record_migration(conn, MIGRATION_006_ID, MIGRATION_006_CHECKSUM, MIGRATION_006_DESCRIPTION)?;
    apply_logical_symbol_groups(conn)?;
    record_migration(conn, MIGRATION_007_ID, MIGRATION_007_CHECKSUM, MIGRATION_007_DESCRIPTION)?;
    apply_commit_addressable_worktrees(conn)?;
    record_migration(conn, MIGRATION_008_ID, MIGRATION_008_CHECKSUM, MIGRATION_008_DESCRIPTION)?;
    apply_github_ref_sync(conn)?;
    record_migration(conn, MIGRATION_009_ID, MIGRATION_009_CHECKSUM, MIGRATION_009_DESCRIPTION)?;
    apply_symbol_facts(conn)?;
    record_migration(conn, MIGRATION_010_ID, MIGRATION_010_CHECKSUM, MIGRATION_010_DESCRIPTION)?;
    apply_repo_memories(conn)?;
    record_migration(conn, MIGRATION_011_ID, MIGRATION_011_CHECKSUM, MIGRATION_011_DESCRIPTION)?;
    apply_repo_memory_call_paths(conn)?;
    record_migration(conn, MIGRATION_012_ID, MIGRATION_012_CHECKSUM, MIGRATION_012_DESCRIPTION)?;
    apply_graph_file_lookup_indexes(conn)?;
    record_migration(conn, MIGRATION_013_ID, MIGRATION_013_CHECKSUM, MIGRATION_013_DESCRIPTION)?;
    apply_memory_binding_signals(conn)?;
    record_migration(conn, MIGRATION_014_ID, MIGRATION_014_CHECKSUM, MIGRATION_014_DESCRIPTION)?;
    apply_repo_memory_call_path_edges(conn)?;
    record_migration(conn, MIGRATION_015_ID, MIGRATION_015_CHECKSUM, MIGRATION_015_DESCRIPTION)?;
    apply_symbol_line_spans(conn)?;
    record_migration(conn, MIGRATION_016_ID, MIGRATION_016_CHECKSUM, MIGRATION_016_DESCRIPTION)?;
    apply_edge_callee_byte_range(conn)?;
    record_migration(conn, MIGRATION_017_ID, MIGRATION_017_CHECKSUM, MIGRATION_017_DESCRIPTION)?;
    apply_oracle_tables(conn)?;
    record_migration(conn, MIGRATION_018_ID, MIGRATION_018_CHECKSUM, MIGRATION_018_DESCRIPTION)?;
    apply_scip_moniker_anchors(conn)?;
    record_migration(conn, MIGRATION_019_ID, MIGRATION_019_CHECKSUM, MIGRATION_019_DESCRIPTION)?;
    apply_edge_string_interning(conn)?;
    record_migration(conn, MIGRATION_020_ID, MIGRATION_020_CHECKSUM, MIGRATION_020_DESCRIPTION)?;
    apply_symbol_scope_path(conn)?;
    record_migration(conn, MIGRATION_021_ID, MIGRATION_021_CHECKSUM, MIGRATION_021_DESCRIPTION)?;
    apply_per_package_import_scope(conn)?;
    record_migration(conn, MIGRATION_022_ID, MIGRATION_022_CHECKSUM, MIGRATION_022_DESCRIPTION)?;
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
                message: "legacy index schema has no schema_version table; run `rag-rat migrate` \
                          or rebuild the derived index with `rag-rat index --full`"
                    .to_string(),
            }
        } else {
            SchemaStatus {
                state: SchemaState::Missing,
                current_version: 0,
                latest_version: LATEST_SCHEMA_VERSION,
                migrations: Vec::new(),
                message: "index schema is not initialized; run `rag-rat migrate` or build the \
                          derived index with `rag-rat index --full`"
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
            message: "index schema is older than this rag-rat; run `rag-rat migrate` or rebuild \
                      the derived index with `rag-rat index --full`"
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

pub fn check_compatible(conn: &Connection) -> anyhow::Result<()> {
    let status = status(conn)?;
    match status.state {
        SchemaState::Compatible => Ok(()),
        SchemaState::Missing => {
            anyhow::bail!(
                "{}",
                "index schema is not initialized; run `rag-rat migrate`, `rag-rat index`, or \
                 `rag-rat index --full`"
            )
        },
        SchemaState::Older => anyhow::bail!("{}", status.message),
        SchemaState::Newer => anyhow::bail!("{}", status.message),
        SchemaState::Dirty => anyhow::bail!("{}", status.message),
    }
}
