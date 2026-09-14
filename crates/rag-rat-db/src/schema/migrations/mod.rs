//! The additive migration ladder's infrastructure: the `schema_version` ledger reads and writes,
//! the migration provenance stamp, the recognizers that map ledger rows onto the shipped roster,
//! and the DDL guard helpers every step uses. The step bodies live in [`steps`]; the index below
//! re-exports the ones the roster and the rest of the crate call.

use rusqlite::{Connection, OptionalExtension, params};

use super::AppliedMigration;

mod steps;

// The step index: every function the roster names, plus the step helpers the rest of the crate
// (the baseline, the ladder, the engine's bootstrap fixtures) calls.
pub(crate) use steps::clones::apply_clone_subblock_postings_tables;
pub use steps::clones::{apply_clone_fingerprint_tables, apply_clone_graph_tables};
pub use steps::distill::{
    apply_distill_anchor_selection, apply_distill_enriched_context,
    apply_distill_evidence_source_part, apply_distill_record_store,
    apply_distill_safe_input_snapshot, apply_papertrail_distill_substrate,
};
pub use steps::dream_tables::{
    apply_memory_model_failures_table, apply_memory_verification_tables,
};
pub use steps::github_keys::apply_github_child_key_widening;
pub(crate) use steps::github_keys::apply_github_natural_key_widening;
pub(crate) use steps::graph::{
    apply_chunk_text_compression_tables, apply_contentless_chunk_fts,
    apply_derived_artifact_reconcile_metadata, apply_drop_chunks_text,
    apply_edge_callee_byte_range, apply_edge_evidence_and_resolution,
    apply_edge_source_target_spans, apply_edges_hidden_flag, apply_edges_view_refresh,
    apply_edges_view_scalar_suppression, apply_embedding_policy_and_input_hash,
    apply_embedding_vector_metadata, apply_github_ref_sync, apply_graph_file_lookup_indexes,
    apply_logical_group_reason_by_evidence, apply_logical_symbol_groups,
    apply_memory_binding_signals, apply_per_package_import_scope, apply_repo_memories,
    apply_repo_memory_call_path_edges, apply_repo_memory_call_paths, apply_symbol_facts,
    apply_symbol_line_spans, apply_symbol_scope_path, ensure_edges_data_indexes, ensure_edges_view,
    migrate_chunks, migrate_edges, migrate_files,
};
pub use steps::graph::{
    apply_edge_string_interning, apply_edge_target_qname_index, apply_external_symbols,
    apply_files_has_test_code, apply_oracle_tables, apply_scip_moniker_anchors,
};
pub(crate) use steps::index_enrichment::{
    apply_clone_refinements_lcs_sampled, apply_commit_addressable_worktrees,
    apply_edge_oracle_content_anchor, apply_embedding_content_cache,
    apply_intern_symbol_qualified_names, apply_symbols_is_test, apply_token_bag_blob,
};
pub use steps::index_enrichment::{apply_dream_findings, apply_git_change_couplings};
pub use steps::memory_and_clones::{apply_clone_delta_maintenance, apply_clone_df_epoch};
pub(crate) use steps::memory_and_clones::{apply_memory_payload_json, apply_repo_node_edges};
pub use steps::papertrail::{
    apply_papertrail_binding_health, apply_papertrail_mirror_resume_state,
    apply_papertrail_provider_neutral_schema,
};
pub(crate) use steps::papertrail::{
    apply_papertrail_comment_cursor, apply_papertrail_ref_item_kind, create_papertrail_tables,
};
pub use steps::path_spelling::{
    V097_PATH_VALUED_META_KEYS, V097_WORKTREE_ID_SCOPED_TABLES,
    apply_reindex_after_unix_backslash_rendering, apply_windows_verbatim_path_rekey,
    rekey_persisted_path_spellings,
};
pub use steps::repo_scoping::{
    apply_files_generation, apply_github_repo_id_scoping, apply_move_per_repo_meta,
    apply_repo_id_core_scoping, apply_repo_id_periphery_scoping, apply_repos_registry,
    rebuild_repo_memory_fts_with_repo_id,
};
pub use steps::sync_substrate::{
    apply_account_authority_boundaries, apply_account_authority_projection,
    apply_account_candidate_dag, apply_chunk_symbol_id, apply_content_candidate_dag,
    apply_content_projected_tables, apply_content_refold_queue_and_stats,
    apply_content_streams_pending_refold, apply_oplog_device_identity, apply_oplog_device_x25519,
    apply_oplog_local_account, apply_oplog_storage, apply_oplog_stream_scoping,
};
pub(crate) use steps::sync_substrate::{
    apply_binding_downgrade_marker, apply_sync_security_events,
};
pub(crate) use steps::syncable_tables::{
    apply_content_author_stream_index, apply_content_entries_lamport_column,
    apply_content_projected_node_anchors, apply_content_projected_node_source_hash,
    apply_refold_account_authority_projections, apply_refold_content_streams_for_lamport_clamp,
    apply_refold_for_concurrent_cut_vouch, apply_refold_for_held_control_log_freshness,
    apply_writer_invites, ensure_content_projection_shape,
};
pub use steps::syncable_tables::{
    apply_content_projected_superseded_anchors, apply_file_graph_version_provenance,
    apply_memory_applied_anchor_snapshot, apply_memory_binding_resolution,
    apply_memory_note_summaries, apply_memory_parked_anchor_baselines,
    apply_readoption_audit_nullable_winner, apply_receiver_type_hint_interning,
    apply_syncable_distill_anchors, apply_syncable_distill_edges_and_alternatives,
    apply_syncable_distill_evidence, apply_syncable_distill_record_commits,
    apply_syncable_distill_records, apply_syncable_memory_bindings, apply_syncable_overlay_tables,
    apply_table_sync_readoption, apply_table_sync_retained_floors, apply_tombstone_statements,
};
pub use steps::table_sync::{
    apply_account_candidate_reservation_targets, apply_account_candidate_reservations,
    apply_clone_postings_row_count, apply_content_digest_state, apply_lens_enrichment_revision,
    apply_lens_lane_revisions, apply_sync_invites, apply_sync_invites_normalized_receipts,
    apply_sync_origin_and_edge_tombstone, apply_table_sync_gapped_entries,
    apply_table_sync_projection_state, apply_table_sync_repo_incarnations,
    apply_table_sync_spec_version, apply_table_sync_tables,
};

pub(crate) fn applied_migrations(conn: &Connection) -> anyhow::Result<Vec<AppliedMigration>> {
    let mut stmt = conn.prepare(
        "
        SELECT id, applied_at_ms, checksum, description
        FROM schema_version
        ORDER BY applied_at_ms, id
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(AppliedMigration {
            id: row.get(0)?,
            applied_at_ms: row.get(1)?,
            checksum: row.get(2)?,
            description: row.get(3)?,
        })
    })?;
    let mut migrations = Vec::new();
    for row in rows {
        migrations.push(row?);
    }
    Ok(migrations)
}

pub(crate) fn known_version(migrations: &[AppliedMigration]) -> u32 {
    migrations.iter().filter_map(|migration| shipped_version(&migration.id)).max().unwrap_or(0)
}

pub(crate) fn known_migration(id: &str) -> bool {
    shipped_version(id).is_some() || id == super::DIRTY_MIGRATION_ID
}

pub(crate) fn migration_checksum_mismatch(migration: &AppliedMigration) -> bool {
    shipped_checksum(&migration.id).is_some_and(|checksum| migration.checksum != checksum)
}

/// The ladder position a shipped migration id maps to: the baseline (001) is 1, and each
/// [`ADDITIVE_MIGRATIONS`](super::ADDITIVE_MIGRATIONS) entry is its 1-based position after it.
/// `None` for a row written by a future binary (or the dirty marker, which has no version).
fn shipped_version(id: &str) -> Option<u32> {
    if id == super::MIGRATION_001_ID {
        return Some(1);
    }
    super::ADDITIVE_MIGRATIONS
        .iter()
        .position(|step| step.id == id)
        .and_then(|index| u32::try_from(index + 2).ok())
}

fn shipped_checksum(id: &str) -> Option<&'static str> {
    if id == super::MIGRATION_001_ID {
        return Some(super::MIGRATION_001_CHECKSUM);
    }
    super::ADDITIVE_MIGRATIONS.iter().find(|step| step.id == id).map(|step| step.checksum)
}

pub(crate) fn record_migration(
    conn: &Connection,
    id: &str,
    checksum: &str,
    description: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO schema_version(id, applied_at_ms, checksum, description)
         VALUES (?1, ?2, ?3, ?4)",
        params![id, rag_rat_base::time::now_ms(), checksum, description],
    )?;
    Ok(())
}

/// GLOBAL `index_meta` keys recording WHO last brought this store's schema current (#585). About
/// the DB file, not a repo — so they live in `index_meta`, not `repo_meta`. Overwritten each time a
/// migration/create runs, so a stranded fleet is diagnosable in one query instead of forensics.
pub(crate) const MIGRATION_PROVENANCE_KEYS: &[&str] = &[
    "last_migration_binary_version",
    "last_migration_binary_exe",
    "last_migration_to_version",
    "last_migration_at_ms",
];

/// Stamp the migration-provenance keys after the schema is brought current. The binary version is
/// [`crate::binary_version`] (the CLI's git-stamped `RAG_RAT_VERSION`, else `CARGO_PKG_VERSION`),
/// so a dev build that migrates a shared store leaves a `+g<hash>` fingerprint that names it.
///
/// ONE atomic multi-row upsert: statement-level atomicity means a mid-write failure (e.g. a
/// concurrent writer's `SQLITE_BUSY`) leaves NO partial provenance — all four keys land or none do,
/// so the reader never sees a half-written record. Call sites treat a failure here as best-effort
/// (the migration already committed; provenance is diagnostic), so a stamp failure never fails the
/// migration — it just leaves the record absent until the next one, which the reader handles.
pub(crate) fn record_migration_provenance(conn: &Connection) -> rusqlite::Result<()> {
    let exe =
        std::env::current_exe().ok().map(|path| path.display().to_string()).unwrap_or_default();
    conn.execute(
        "INSERT INTO index_meta(key, value) VALUES (?1, ?2), (?3, ?4), (?5, ?6), (?7, ?8) ON \
         CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![
            MIGRATION_PROVENANCE_KEYS[0],
            rag_rat_base::version::binary_version(),
            MIGRATION_PROVENANCE_KEYS[1],
            exe,
            MIGRATION_PROVENANCE_KEYS[2],
            super::LATEST_SCHEMA_VERSION.to_string(),
            MIGRATION_PROVENANCE_KEYS[3],
            rag_rat_base::time::now_ms().to_string(),
        ],
    )?;
    Ok(())
}

/// A human note naming who last migrated this store, appended to the `Newer` refusal (#585). Empty
/// when no provenance is recorded (an older DB, or one never migrated by a provenance-aware
/// binary). Defensive: any read error yields "" — `status` must never fail on a weird DB.
pub(crate) fn migration_provenance_note(conn: &Connection) -> String {
    let read = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .filter(|value| !value.is_empty())
    };
    match (read("last_migration_binary_version"), read("last_migration_binary_exe")) {
        (Some(version), Some(exe)) => {
            format!("; the index was last migrated by rag-rat {version} at {exe}")
        },
        (Some(version), None) => format!("; the index was last migrated by rag-rat {version}"),
        _ => String::new(),
    }
}

pub fn table_exists(conn: &Connection, table: &str) -> anyhow::Result<bool> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'virtual table') AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(exists)
}

pub(crate) fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    if column_exists(conn, table, column)? {
        return Ok(());
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))
}

pub fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a `kind`-typed object (`"table"`, `"index"`, …) named `name` exists in `sqlite_master`.
/// A plain table and an FTS5 virtual table both register as `type = 'table'`, so `"table"` finds
/// either.
pub(crate) fn sqlite_object_exists(
    conn: &Connection,
    kind: &str,
    name: &str,
) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2",
            [kind, name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn table_is_strict(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT strict FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
        [table],
        |row| row.get(0),
    )
}

fn primary_key_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT name FROM pragma_table_info(?1) WHERE pk > 0 ORDER BY pk")?;
    stmt.query_map([table], |row| row.get(0))?.collect()
}

#[cfg(test)]
#[path = "tests/lens_lane_revision_migration_tests.rs"]
mod lens_lane_revision_migration_tests;

#[cfg(test)]
#[path = "tests/syncable_overlay_migration_tests.rs"]
mod syncable_overlay_migration_tests;

#[cfg(test)]
#[path = "tests/table_sync_repo_incarnation_migration_tests.rs"]
mod table_sync_repo_incarnation_migration_tests;

#[cfg(test)]
#[path = "tests/windows_verbatim_rekey_tests.rs"]
mod windows_verbatim_rekey_tests;

#[cfg(test)]
#[path = "tests/memory_model_failure_migration_tests.rs"]
mod memory_model_failure_migration_tests;

#[cfg(test)]
#[path = "tests/sync_security_events_migration_tests.rs"]
mod sync_security_events_migration_tests;

#[cfg(test)]
#[path = "tests/reindex_after_unix_backslash_rendering_tests.rs"]
mod reindex_after_unix_backslash_rendering_tests;
