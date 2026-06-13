mod baseline;
mod migrations;
pub(crate) use baseline::*;
pub(crate) use migrations::*;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub const LATEST_SCHEMA_VERSION: u32 = 21;
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
    // The additive migrations on top of the baseline, in order. Same list `migrate_forward` walks,
    // so a fresh DB (here) and an existing DB behind by N apply exactly the same steps.
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
];

/// Apply ONLY the additive migrations not already recorded, in order — the forward-only path for an
/// existing index that lags this binary. Unlike [`apply`], it never re-runs an already-applied
/// migration, so a data backfill like 005's resolution rewrite (an unconditional UPDATE) cannot
/// clobber current values on a routine open. The caller guarantees the baseline (001) is present (a
/// versioned ledger exists); a ledger-less legacy DB is refused upstream, not force-migrated.
pub fn migrate_forward(conn: &Connection) -> anyhow::Result<()> {
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
