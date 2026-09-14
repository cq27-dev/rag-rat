use rusqlite::Connection;

use super::graph::ensure_edges_view;
use crate::schema::migrations::{
    add_column_if_missing, column_exists, primary_key_columns, sqlite_object_exists,
    table_is_strict,
};

/// V100 (#976): intern the Rust receiver-type hint and make call-path identity target-aware.
///
/// Conservative receiver-type inference records the type a method call was made ON, so resolution
/// can bind `worker.run()` to `Worker::run` instead of every `run` in the repo. The value is an
/// interned `name_strings` id, like the sibling name columns, because the same handful of type
/// paths repeat across every call site in a file. The `edges` view is rebuilt so readers see the
/// new column without a second migration.
pub fn apply_receiver_type_hint_interning(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "edges_data", "receiver_type_hint_id", "INTEGER")?;
    add_column_if_missing(
        conn,
        "repo_memory_call_path_edges",
        "callee_logical_symbol_id",
        "INTEGER",
    )?;
    add_column_if_missing(
        conn,
        "repo_memory_call_path_edges",
        "callee_identity_known",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_edges_view(conn)?;
    Ok(())
}

/// V101 (#1014): record which graph extractor version produced each file row.
///
/// Existing rows inherit the repository stamp they previously relied on. A later extractor bump
/// can then mark only rows whose exact bytes are readable from the active checkout, leaving a
/// divergent linked-worktree row owed until that checkout opens the shared database itself.
pub fn apply_file_graph_version_provenance(conn: &Connection) -> rusqlite::Result<()> {
    let had_graph_version = column_exists(conn, "files", "graph_version")?;
    let had_scope_version = column_exists(conn, "files", "scope_version")?;
    add_column_if_missing(conn, "files", "graph_version", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "files", "scope_version", "INTEGER NOT NULL DEFAULT 0")?;
    let has_repo_meta = sqlite_object_exists(conn, "table", "repo_meta")?;
    if !had_graph_version && column_exists(conn, "files", "repo_id")? && has_repo_meta {
        conn.execute(
            "UPDATE files
             SET graph_version = COALESCE((
                 SELECT CAST(value AS INTEGER)
                 FROM repo_meta
                 WHERE repo_meta.repo_id = files.repo_id
                   AND repo_meta.key = 'graph_index_version'
             ), 0)",
            [],
        )?;
    }
    if !had_scope_version && column_exists(conn, "files", "repo_id")? && has_repo_meta {
        conn.execute(
            "UPDATE files
             SET scope_version = COALESCE((
                 SELECT CAST(value AS INTEGER)
                 FROM repo_meta
                 WHERE repo_meta.repo_id = files.repo_id
                   AND repo_meta.key = 'logical_key_version'
             ), 0)",
            [],
        )?;
    }
    if column_exists(conn, "files", "repo_id")? && column_exists(conn, "files", "generation")? {
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_files_repo_generation_graph_version
                 ON files(repo_id, generation, graph_version);
             CREATE INDEX IF NOT EXISTS idx_files_repo_generation_scope_version
                 ON files(repo_id, generation, scope_version);",
        )?;
    }
    Ok(())
}

/// V103 (#1109): make memory bindings a deterministic whole-row table for `anchors/1`.
///
/// The old shape was non-STRICT, keyed without `repo_id`, and cascaded from `repo_memories`.
/// Those properties are incompatible with table sync: repository identity must be enforced by the
/// row key, cross-row constraints make LWW arrival-order-dependent, and remote inserts omit
/// checkout-local resolution state. The rebuilt table therefore has no FK or triggers and gives the
/// one non-null local column a deterministic default. Parent cleanup is explicit in the memory
/// drain after this migration.
pub fn apply_syncable_memory_bindings(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memory_bindings_lens_revision_insert;
         DROP TRIGGER IF EXISTS memory_bindings_lens_revision_delete;
         DROP TRIGGER IF EXISTS memory_bindings_lens_revision_update;
         DROP TRIGGER IF EXISTS memory_bindings_lane_revision_insert;
         DROP TRIGGER IF EXISTS memory_bindings_lane_revision_delete;
         DROP TRIGGER IF EXISTS memory_bindings_lane_revision_update;",
    )?;
    if table_is_strict(conn, "repo_memory_bindings")?
        && primary_key_columns(conn, "repo_memory_bindings")?
            == ["repo_id", "memory_id", "binding_kind", "binding_id"]
    {
        return Ok(());
    }

    conn.execute_batch(
        "DROP TABLE IF EXISTS repo_memory_bindings_v103;
         CREATE TABLE repo_memory_bindings_v103(
             repo_id TEXT NOT NULL DEFAULT '__unassigned__',
             memory_id TEXT NOT NULL,
             binding_kind TEXT NOT NULL,
             binding_id TEXT NOT NULL,
             path TEXT,
             start_line INTEGER,
             end_line INTEGER,
             logical_symbol_id INTEGER,
             symbol_id INTEGER,
             chunk_id INTEGER,
             edge_id INTEGER,
             commit_hash TEXT,
             tracker TEXT,
             project TEXT,
             item_key TEXT,
             anchor_status TEXT NOT NULL DEFAULT 'unverified',
             created_at_ms INTEGER NOT NULL,
             symbol_kind TEXT,
             signature_hash TEXT,
             moniker_tool TEXT,
             moniker_tool_version TEXT,
             relocation_reason TEXT,
             downgrade_pending_at_ms INTEGER,
             PRIMARY KEY(repo_id, memory_id, binding_kind, binding_id)
         ) STRICT;
         INSERT INTO repo_memory_bindings_v103(
             repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
             logical_symbol_id, symbol_id, chunk_id, edge_id, commit_hash, tracker, project,
             item_key, anchor_status, created_at_ms, symbol_kind, signature_hash, moniker_tool,
             moniker_tool_version, relocation_reason, downgrade_pending_at_ms
         )
         SELECT repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
             logical_symbol_id, symbol_id, chunk_id, edge_id, commit_hash, tracker, project,
             item_key, anchor_status, created_at_ms, symbol_kind, signature_hash, moniker_tool,
             moniker_tool_version, relocation_reason, downgrade_pending_at_ms
         FROM repo_memory_bindings;
         DROP TABLE repo_memory_bindings;
         ALTER TABLE repo_memory_bindings_v103 RENAME TO repo_memory_bindings;
         CREATE INDEX idx_repo_memory_bindings_logical_symbol
             ON repo_memory_bindings(logical_symbol_id);
         CREATE INDEX idx_repo_memory_bindings_symbol ON repo_memory_bindings(symbol_id);
         CREATE INDEX idx_repo_memory_bindings_chunk ON repo_memory_bindings(chunk_id);
         CREATE INDEX idx_repo_memory_bindings_edge ON repo_memory_bindings(edge_id);
         CREATE INDEX idx_repo_memory_bindings_path ON repo_memory_bindings(path);",
    )
}
/// V104 (#997): the durable re-adoption worklist and audit log.
///
/// An effective `DeviceRemove` makes the #935 ingest gate refuse every later copy of that
/// device's entries, so rows whose whole-row LWW winner is the removed writer never reach a
/// replica enrolled after the removal. The account fold records each removal here, and a
/// roster-effective writer drains the worklist by re-authoring the surviving state under its
/// own chain. `roster_ref` is NOT a foreign key: the projection deletes and rewrites roster
/// history on every fold, and the worklist must survive that rewrite.
pub fn apply_table_sync_readoption(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_readoption_work(
              account_id BLOB NOT NULL CHECK(length(account_id) = 32),
              device_fingerprint BLOB NOT NULL CHECK(length(device_fingerprint) = 32),
              stream_id BLOB NOT NULL CHECK(length(stream_id) = 32),
              roster_ref BLOB NOT NULL CHECK(length(roster_ref) = 32),
              removed_at_epoch INTEGER NOT NULL,
              enqueued_at_ms INTEGER NOT NULL,
              processed_at_ms INTEGER,
              PRIMARY KEY(account_id, device_fingerprint, stream_id)
          ) STRICT;
          CREATE INDEX IF NOT EXISTS table_sync_readoption_work_pending
              ON table_sync_readoption_work(account_id, stream_id)
              WHERE processed_at_ms IS NULL;
          CREATE TABLE IF NOT EXISTS table_sync_readoption_audit(
              audit_id INTEGER PRIMARY KEY,
              account_id BLOB NOT NULL CHECK(length(account_id) = 32),
              removed_fingerprint BLOB NOT NULL CHECK(length(removed_fingerprint) = 32),
              adopter_fingerprint BLOB NOT NULL CHECK(length(adopter_fingerprint) = 32),
              stream_id BLOB NOT NULL CHECK(length(stream_id) = 32),
              repo_id TEXT NOT NULL,
              scope_id TEXT NOT NULL,
              table_name TEXT NOT NULL,
              row_pk TEXT NOT NULL,
              original_lamport INTEGER NOT NULL,
              original_entry_hash BLOB NOT NULL CHECK(length(original_entry_hash) = 32),
              adopted_entry_hash BLOB NOT NULL CHECK(length(adopted_entry_hash) = 32),
              adopted_at_ms INTEGER NOT NULL
          ) STRICT;
          CREATE INDEX IF NOT EXISTS table_sync_readoption_audit_stream
              ON table_sync_readoption_audit(account_id, stream_id);",
    )
}

/// V105 (#1127): the per-(stream, device) retained floor.
///
/// Accepted-entry compaction drops a chain prefix below the floor; this table is how a peer
/// (and the local accept path) tells an intentionally reclaimed prefix from a chain gap. It is
/// swept with the stream directory on repository purge, NOT retained like the chain-tip
/// witnesses: once the accepted log itself is gone, "the prefix below F was compacted" stops
/// being true, and a surviving floor would short-circuit the re-offered prefix the restored
/// chain then waits on forever.
pub fn apply_table_sync_retained_floors(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS table_sync_retained_floors(
             stream_id          BLOB    NOT NULL CHECK(length(stream_id) = 32),
             device_fingerprint BLOB    NOT NULL CHECK(length(device_fingerprint) = 32),
             lamport            INTEGER NOT NULL,
             entry_hash         BLOB    NOT NULL CHECK(length(entry_hash) = 32),
             compacted_at_ms    INTEGER NOT NULL,
             PRIMARY KEY(stream_id, device_fingerprint)
         ) STRICT;",
    )
}

/// V106 (#1127): re-adoption audit provenance for a compacted winner.
///
/// Re-adoption candidates derive from the merge state, which survives compaction; a winning entry
/// below the retained floor is gone, so the audit records its slot by `(stream, device, lamport)`
/// and stores NULL for the hash. Rebuilds the just-shipped V104 table — it holds only locally
/// authored audit rows, copied verbatim.
pub fn apply_readoption_audit_nullable_winner(conn: &Connection) -> rusqlite::Result<()> {
    let already_nullable: bool = conn.query_row(
        "SELECT NOT \"notnull\" FROM pragma_table_info('table_sync_readoption_audit')
         WHERE name = 'original_entry_hash'",
        [],
        |row| row.get(0),
    )?;
    if already_nullable {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE TABLE table_sync_readoption_audit_v106(
             audit_id INTEGER PRIMARY KEY,
             account_id BLOB NOT NULL CHECK(length(account_id) = 32),
             removed_fingerprint BLOB NOT NULL CHECK(length(removed_fingerprint) = 32),
             adopter_fingerprint BLOB NOT NULL CHECK(length(adopter_fingerprint) = 32),
             stream_id BLOB NOT NULL CHECK(length(stream_id) = 32),
             repo_id TEXT NOT NULL,
             scope_id TEXT NOT NULL,
             table_name TEXT NOT NULL,
             row_pk TEXT NOT NULL,
             original_lamport INTEGER NOT NULL,
             original_entry_hash BLOB CHECK(
                 original_entry_hash IS NULL OR length(original_entry_hash) = 32
             ),
             adopted_entry_hash BLOB NOT NULL CHECK(length(adopted_entry_hash) = 32),
             adopted_at_ms INTEGER NOT NULL
         ) STRICT;
         INSERT INTO table_sync_readoption_audit_v106
         SELECT * FROM table_sync_readoption_audit;
         DROP TABLE table_sync_readoption_audit;
         ALTER TABLE table_sync_readoption_audit_v106
             RENAME TO table_sync_readoption_audit;
         CREATE INDEX IF NOT EXISTS table_sync_readoption_audit_stream
             ON table_sync_readoption_audit(account_id, stream_id);",
    )
}

/// V107 (#1133): make `memory_reality` and `memory_summaries` syncable on the `overlay/1` scope by
/// dropping their Lens revision triggers.
///
/// Both tables carry two trigger families that bump a per-repo Lens lane on every physical row
/// write: the V093 `*_lens_revision_*` set (the aggregate enrichment clock) and the V102
/// `*_lane_revision_*` set (the split memories lane). Under `overlay/1` a row arrives by whole-row
/// LWW apply, and a trigger firing on a wire-applied row is a device-local side effect that also
/// fires on redeliveries, losing writes, and refold replay — exactly what the sync apply must not
/// do. The dream write path and the sync apply path advance the enrichment and memories lanes
/// explicitly instead (`meta::bump_lens_revisions`), so the only remaining lane movement is the one
/// the code chooses.
///
/// The DROP is UNCONDITIONAL: `schema::apply` replays the whole additive ladder, so V093/V102
/// recreate these triggers ahead of this migration on every `index --full`; short-circuiting on
/// table shape would let the recreated triggers survive the replay.
pub fn apply_syncable_overlay_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memory_reality_lens_revision_insert;
         DROP TRIGGER IF EXISTS memory_reality_lens_revision_delete;
         DROP TRIGGER IF EXISTS memory_reality_lens_revision_update;
         DROP TRIGGER IF EXISTS memory_reality_lane_revision_insert;
         DROP TRIGGER IF EXISTS memory_reality_lane_revision_delete;
         DROP TRIGGER IF EXISTS memory_reality_lane_revision_update;
         DROP TRIGGER IF EXISTS memory_summaries_lens_revision_insert;
         DROP TRIGGER IF EXISTS memory_summaries_lens_revision_delete;
         DROP TRIGGER IF EXISTS memory_summaries_lens_revision_update;
         DROP TRIGGER IF EXISTS memory_summaries_lane_revision_insert;
         DROP TRIGGER IF EXISTS memory_summaries_lane_revision_delete;
         DROP TRIGGER IF EXISTS memory_summaries_lane_revision_update;",
    )
}

/// V108 (#1135): make `papertrail_distill` a deterministic whole-row table for `distill/1`.
///
/// The old shape keyed on an AUTOINCREMENT `id` (device-local, non-deterministic) with the thread
/// natural key only as a UNIQUE index — incompatible with whole-row LWW sync. The rebuilt table
/// keys on `(repo_id, tracker, project, item_kind, item_key)` (repo_id part of the PK, as the
/// transport requires), drops `id`, has no FK or triggers, and adds the `CHECK(x IN (0,1))` the
/// store lint asks for on the genuine boolean facets (NOT on `quotes_materialized`/
/// `anchors_qualified_count`, which are counts). Children reference the thread by its natural key,
/// never `id`, so dropping it breaks nothing.
///
/// The trigger DROP is UNCONDITIONAL and precedes the shape short-circuit: `schema::apply` replays
/// the whole ladder, so V093/V102 recreate the papertrail-lane triggers ahead of this migration on
/// every `index --full`. Under `distill/1` a row arrives by whole-row LWW, so the distill write and
/// the sync apply advance the papertrail Lens lane explicitly instead of via a trigger.
pub fn apply_syncable_distill_records(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS papertrail_distill_lens_revision_insert;
         DROP TRIGGER IF EXISTS papertrail_distill_lens_revision_delete;
         DROP TRIGGER IF EXISTS papertrail_distill_lens_revision_update;
         DROP TRIGGER IF EXISTS papertrail_distill_lane_revision_insert;
         DROP TRIGGER IF EXISTS papertrail_distill_lane_revision_delete;
         DROP TRIGGER IF EXISTS papertrail_distill_lane_revision_update;",
    )?;
    if table_is_strict(conn, "papertrail_distill")?
        && primary_key_columns(conn, "papertrail_distill")?
            == ["repo_id", "tracker", "project", "item_kind", "item_key"]
    {
        return Ok(());
    }

    conn.execute_batch(
        "DROP TABLE IF EXISTS papertrail_distill_v108;
         CREATE TABLE papertrail_distill_v108(
             tracker TEXT NOT NULL,
             project TEXT NOT NULL,
             item_kind TEXT NOT NULL,
             item_key TEXT NOT NULL,
             distill_input_hash TEXT NOT NULL,
             pipeline_version INTEGER NOT NULL,
             root_issue TEXT,
             root_cause TEXT,
             root_cause_class TEXT,
             decision_chosen TEXT,
             outcome_summary TEXT,
             outcome_status_model TEXT,
             epistemic_status_decision TEXT,
             epistemic_status_outcome TEXT,
             fix_edge_source TEXT NOT NULL,
             -- COUNTS, not booleans (quotes_materialized is the evidence-unit count) — no 0/1 \
         CHECK.
             quotes_materialized INTEGER NOT NULL DEFAULT 0,
             anchors_qualified_count INTEGER NOT NULL DEFAULT 0,
             thread_shape TEXT NOT NULL,
             -- Genuine 0/1 facets: carry the CHECK the store lint asks for (Bool has no pragma the
             -- lint can require it through).
             outcome_claim_verified INTEGER NOT NULL DEFAULT 0
                 CHECK(outcome_claim_verified IN (0, 1)),
             decision_provenance_verified INTEGER NOT NULL DEFAULT 0
                 CHECK(decision_provenance_verified IN (0, 1)),
             revert_override INTEGER NOT NULL DEFAULT 0 CHECK(revert_override IN (0, 1)),
             closing_keyword_floor TEXT,
             distilled_at_ms INTEGER NOT NULL,
             repo_id TEXT NOT NULL DEFAULT '__unassigned__',
             prompt_version INTEGER,
             model_input_hash TEXT,
             PRIMARY KEY(repo_id, tracker, project, item_kind, item_key)
         ) STRICT;
         INSERT INTO papertrail_distill_v108(
             tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
             root_issue, root_cause, root_cause_class, decision_chosen, outcome_summary,
             outcome_status_model, epistemic_status_decision, epistemic_status_outcome,
             fix_edge_source, quotes_materialized, anchors_qualified_count, thread_shape,
             outcome_claim_verified, decision_provenance_verified, revert_override,
             closing_keyword_floor, distilled_at_ms, repo_id, prompt_version, model_input_hash)
         SELECT
             tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
             root_issue, root_cause, root_cause_class, decision_chosen, outcome_summary,
             outcome_status_model, epistemic_status_decision, epistemic_status_outcome,
             fix_edge_source, quotes_materialized, anchors_qualified_count, thread_shape,
             outcome_claim_verified, decision_provenance_verified, revert_override,
             closing_keyword_floor, distilled_at_ms, repo_id, prompt_version, model_input_hash
         FROM papertrail_distill;
         DROP TABLE papertrail_distill;
         ALTER TABLE papertrail_distill_v108 RENAME TO papertrail_distill;",
    )
}

/// V109 (#1137): make the distill `edges` and `alternatives` children syncable on `distill/1`.
///
/// Like the parent (V108), each keyed on a device-local AUTOINCREMENT `id` with its natural key
/// only as a UNIQUE index. Rebuild each onto the natural key (repo_id first), dropping `id`;
/// neither carries triggers, local columns, or FKs. Each block short-circuits once its table is
/// already in the rebuilt shape, so a full-ladder replay is a no-op.
pub fn apply_syncable_distill_edges_and_alternatives(conn: &Connection) -> rusqlite::Result<()> {
    if !(table_is_strict(conn, "papertrail_distill_edges")?
        && primary_key_columns(conn, "papertrail_distill_edges")?
            == [
                "repo_id",
                "tracker",
                "project",
                "src_item_kind",
                "src_item_key",
                "dst_item_kind",
                "dst_item_key",
                "edge_kind",
            ])
    {
        conn.execute_batch(
            "DROP TABLE IF EXISTS papertrail_distill_edges_v109;
             CREATE TABLE papertrail_distill_edges_v109(
                 tracker TEXT NOT NULL,
                 project TEXT NOT NULL,
                 src_item_kind TEXT NOT NULL,
                 src_item_key TEXT NOT NULL,
                 dst_item_kind TEXT NOT NULL,
                 dst_item_key TEXT NOT NULL,
                 edge_kind TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 repo_id TEXT NOT NULL DEFAULT '__unassigned__',
                 PRIMARY KEY(repo_id, tracker, project, src_item_kind, src_item_key,
                             dst_item_kind, dst_item_key, edge_kind)
             ) STRICT;
             INSERT INTO papertrail_distill_edges_v109(
                 tracker, project, src_item_kind, src_item_key, dst_item_kind, dst_item_key,
                 edge_kind, created_at_ms, repo_id)
             SELECT tracker, project, src_item_kind, src_item_key, dst_item_kind, dst_item_key,
                 edge_kind, created_at_ms, repo_id
             FROM papertrail_distill_edges;
             DROP TABLE papertrail_distill_edges;
             ALTER TABLE papertrail_distill_edges_v109 RENAME TO papertrail_distill_edges;",
        )?;
    }
    if !(table_is_strict(conn, "papertrail_distill_alternatives")?
        && primary_key_columns(conn, "papertrail_distill_alternatives")?
            == ["repo_id", "tracker", "project", "item_kind", "item_key", "ordinal"])
    {
        conn.execute_batch(
            "DROP TABLE IF EXISTS papertrail_distill_alternatives_v109;
             CREATE TABLE papertrail_distill_alternatives_v109(
                 tracker TEXT NOT NULL,
                 project TEXT NOT NULL,
                 item_kind TEXT NOT NULL,
                 item_key TEXT NOT NULL,
                 ordinal INTEGER NOT NULL,
                 alternative TEXT NOT NULL,
                 reason TEXT,
                 repo_id TEXT NOT NULL DEFAULT '__unassigned__',
                 PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, ordinal)
             ) STRICT;
             INSERT INTO papertrail_distill_alternatives_v109(
                 tracker, project, item_kind, item_key, ordinal, alternative, reason, repo_id)
             SELECT tracker, project, item_kind, item_key, ordinal, alternative, reason, repo_id
             FROM papertrail_distill_alternatives;
             DROP TABLE papertrail_distill_alternatives;
             ALTER TABLE papertrail_distill_alternatives_v109
                 RENAME TO papertrail_distill_alternatives;",
        )?;
    }
    Ok(())
}

/// V110 (#1139): make the distill `record_commits` child syncable on `distill/1`.
///
/// The table was key-only (its only columns were the natural key + `commit_sha`), which the
/// whole-row apply path rejects — a syncable table needs at least one non-key synced column. The
/// rebuild keys on `(repo_id, tracker, project, item_kind, item_key, commit_sha)` (its former
/// UNIQUE index), drops the device-local AUTOINCREMENT `id`, and adds `created_at_ms` (when the
/// fixing-commit link was recorded) as that non-key column. Legacy rows get `0`; a record
/// regeneration rewrites them with the real timestamp (the mechanical junctions are cleared and
/// re-mined). No triggers, local columns, or FKs. Short-circuits once the table is already in the
/// rebuilt shape.
pub fn apply_syncable_distill_record_commits(conn: &Connection) -> rusqlite::Result<()> {
    if table_is_strict(conn, "papertrail_distill_record_commits")?
        && primary_key_columns(conn, "papertrail_distill_record_commits")?
            == ["repo_id", "tracker", "project", "item_kind", "item_key", "commit_sha"]
    {
        return Ok(());
    }
    conn.execute_batch(
        "DROP TABLE IF EXISTS papertrail_distill_record_commits_v110;
         CREATE TABLE papertrail_distill_record_commits_v110(
             tracker TEXT NOT NULL,
             project TEXT NOT NULL,
             item_kind TEXT NOT NULL,
             item_key TEXT NOT NULL,
             commit_sha TEXT NOT NULL,
             created_at_ms INTEGER NOT NULL DEFAULT 0,
             repo_id TEXT NOT NULL DEFAULT '__unassigned__',
             PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, commit_sha)
         ) STRICT;
         INSERT INTO papertrail_distill_record_commits_v110(
             tracker, project, item_kind, item_key, commit_sha, created_at_ms, repo_id)
         SELECT tracker, project, item_kind, item_key, commit_sha, 0, repo_id
         FROM papertrail_distill_record_commits;
         DROP TABLE papertrail_distill_record_commits;
         ALTER TABLE papertrail_distill_record_commits_v110
             RENAME TO papertrail_distill_record_commits;",
    )
}

/// V111 (#1139): make the distill `evidence` child syncable on `distill/1`.
///
/// The table had no natural unique key — a model can cite the same unit twice for one field, and
/// title/body citations share `source_id` — so a composite discriminator is duplicate-unsafe. The
/// rebuild adds a stable per-thread `ordinal` (assigned at insert in citation order by the drain,
/// backfilled here by `id` order within each thread so existing rows get a deterministic sequence),
/// keys on `(repo_id, tracker, project, item_kind, item_key, ordinal)`, and drops the device-local
/// AUTOINCREMENT `id`. No triggers, local columns, or FKs; `source_part` keeps its CHECK.
/// Short-circuits once the table is already in the rebuilt shape.
pub fn apply_syncable_distill_evidence(conn: &Connection) -> rusqlite::Result<()> {
    if table_is_strict(conn, "papertrail_distill_evidence")?
        && primary_key_columns(conn, "papertrail_distill_evidence")?
            == ["repo_id", "tracker", "project", "item_kind", "item_key", "ordinal"]
    {
        return Ok(());
    }
    conn.execute_batch(
        "DROP TABLE IF EXISTS papertrail_distill_evidence_v111;
         CREATE TABLE papertrail_distill_evidence_v111(
             tracker TEXT NOT NULL,
             project TEXT NOT NULL,
             item_kind TEXT NOT NULL,
             item_key TEXT NOT NULL,
             ordinal INTEGER NOT NULL,
             field TEXT NOT NULL,
             source_kind TEXT NOT NULL,
             source_part TEXT CHECK(source_part IN ('title', 'body', 'comment')),
             source_id TEXT NOT NULL,
             byte_start INTEGER NOT NULL,
             byte_end INTEGER NOT NULL,
             quote TEXT NOT NULL,
             author TEXT,
             author_kind TEXT,
             author_association TEXT,
             unit_created_at_ms INTEGER,
             repo_id TEXT NOT NULL DEFAULT '__unassigned__',
             PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, ordinal)
         ) STRICT;
         INSERT INTO papertrail_distill_evidence_v111(
             tracker, project, item_kind, item_key, ordinal, field, source_kind, source_part,
             source_id, byte_start, byte_end, quote, author, author_kind, author_association,
             unit_created_at_ms, repo_id)
         SELECT
             e.tracker, e.project, e.item_kind, e.item_key,
             (SELECT COUNT(*) FROM papertrail_distill_evidence AS earlier
              WHERE earlier.repo_id = e.repo_id AND earlier.tracker = e.tracker
                AND earlier.project = e.project AND earlier.item_kind = e.item_kind
                AND earlier.item_key = e.item_key AND earlier.id < e.id),
             e.field, e.source_kind, e.source_part, e.source_id, e.byte_start, e.byte_end,
             e.quote, e.author, e.author_kind, e.author_association, e.unit_created_at_ms, \
         e.repo_id
         FROM papertrail_distill_evidence AS e;
         DROP TABLE papertrail_distill_evidence;
         ALTER TABLE papertrail_distill_evidence_v111 RENAME TO papertrail_distill_evidence;",
    )
}

/// V112 (#1139): make the distill `anchors` child syncable on `distill/1`.
///
/// Re-key onto the existing UNIQUE natural key `(repo_id, tracker, project, item_kind, item_key,
/// candidate_ordinal)`, drop the device-local AUTOINCREMENT `id`, and drop the six Lens-revision
/// triggers (3 V093 `*_lens_revision_*` on the enrichment lane, 3 V102 `*_lane_revision_*` on the
/// papertrail lane) unconditionally, before the shape short-circuit, so a full-ladder replay that
/// recreates them still ends trigger-free. The write and apply paths advance the lanes explicitly.
///
/// `logical_symbol_id` and `resolved` stay as columns but are the table's checkout-local resolution
/// state — the registry marks them `local_columns`, so they never replicate. `candidate_ordinal`'s
/// and `selected`'s CHECKs are preserved. The `(repo_id, logical_symbol_id)` symbol index is
/// recreated (the rebuild drops all indexes and it serves the drive-by symbol joins); the former
/// UNIQUE candidate index is intentionally NOT recreated — the PK subsumes it.
pub fn apply_syncable_distill_anchors(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS papertrail_distill_anchors_lens_revision_insert;
         DROP TRIGGER IF EXISTS papertrail_distill_anchors_lens_revision_delete;
         DROP TRIGGER IF EXISTS papertrail_distill_anchors_lens_revision_update;
         DROP TRIGGER IF EXISTS papertrail_distill_anchors_lane_revision_insert;
         DROP TRIGGER IF EXISTS papertrail_distill_anchors_lane_revision_delete;
         DROP TRIGGER IF EXISTS papertrail_distill_anchors_lane_revision_update;",
    )?;
    if table_is_strict(conn, "papertrail_distill_anchors")?
        && primary_key_columns(conn, "papertrail_distill_anchors")?
            == ["repo_id", "tracker", "project", "item_kind", "item_key", "candidate_ordinal"]
    {
        return Ok(());
    }
    conn.execute_batch(
        "DROP TABLE IF EXISTS papertrail_distill_anchors_v112;
         CREATE TABLE papertrail_distill_anchors_v112(
             tracker TEXT NOT NULL,
             project TEXT NOT NULL,
             item_kind TEXT NOT NULL,
             item_key TEXT NOT NULL,
             candidate_ordinal INTEGER NOT NULL DEFAULT 0 CHECK(candidate_ordinal >= 0),
             anchor_kind TEXT NOT NULL,
             logical_symbol_id TEXT,
             file_path TEXT,
             name TEXT NOT NULL,
             resolved INTEGER NOT NULL DEFAULT 0,
             selected INTEGER NOT NULL DEFAULT 0 CHECK(selected IN (0, 1)),
             repo_id TEXT NOT NULL DEFAULT '__unassigned__',
             PRIMARY KEY(repo_id, tracker, project, item_kind, item_key, candidate_ordinal)
         ) STRICT;
         INSERT INTO papertrail_distill_anchors_v112(
             tracker, project, item_kind, item_key, candidate_ordinal, anchor_kind,
             logical_symbol_id, file_path, name, resolved, selected, repo_id)
         SELECT tracker, project, item_kind, item_key, candidate_ordinal, anchor_kind,
             logical_symbol_id, file_path, name, resolved, selected, repo_id
         FROM papertrail_distill_anchors;
         DROP TABLE papertrail_distill_anchors;
         ALTER TABLE papertrail_distill_anchors_v112 RENAME TO papertrail_distill_anchors;
         -- `idx_papertrail_distill_anchors_thread` (repo_id, tracker, project, item_kind, \
         item_key)
         -- is deliberately NOT recreated: it is a strict prefix of the new PK, so the PK autoindex
         -- serves the same thread lookups, and nothing references it by name.
         CREATE INDEX IF NOT EXISTS idx_papertrail_distill_anchors_symbol
             ON papertrail_distill_anchors(repo_id, logical_symbol_id);
         -- Recreate the candidate index as NON-UNIQUE (the PK now enforces the uniqueness the
         -- registry lint would reject a second UNIQUE index for). It is otherwise redundant with \
         the
         -- PK, but V078's backfill keys its 'already applied' guard on this index's existence by
         -- name: without it, a full-ladder replay re-runs V078's id-based backfill against this
         -- id-less rebuilt table and errors. Keeping the name satisfies that guard.
         CREATE INDEX IF NOT EXISTS idx_papertrail_distill_anchors_candidate
             ON papertrail_distill_anchors(repo_id, tracker, project, item_kind, item_key,
                                           candidate_ordinal);
         -- The V078 partial `selected` index (recreated identically; non-unique, so the lint is
         -- satisfied) — keeps the drive-by `selected = 1` lookups indexed and preserves the index
         -- inventory later migrations/tests expect.
         CREATE INDEX IF NOT EXISTS idx_papertrail_distill_anchors_selected
             ON papertrail_distill_anchors(repo_id, tracker, project, item_kind, item_key,
                                           candidate_ordinal) WHERE selected = 1;",
    )
}

/// V113 (#1176): re-judge already-accepted `/3` content under the lamport clamp, and purge what
/// the clamp can never re-admit.
///
/// The `/3` acceptance verdict is only re-derived while a stream sits in the refold queue, so a
/// store that accepted a near-ceiling lamport under a binary predating the clamp would keep the
/// stale accepted bit — and the poisoned projection LWW, and the blocked authoring clock —
/// forever, while a freshly-synced replica parks the same entry and the two diverge. Queue every
/// stream that holds content; the next settle (every index pass and sync drain runs one) re-folds
/// it under the current rules and reprojects.
///
/// Queueing alone is not enough: the hook then DELETES the violating candidates outright, because
/// (a) a merely-parked local chain tail wedges all future authoring on that chain, and (b) a
/// merely-parked over-ceiling envelope is re-advertised to peers that refuse it before storage,
/// retransmitting forever. The lamport sits inside the signed CBOR envelope, which SQL cannot
/// decode — hence the hook. Queue BEFORE purging, so a stream whose every entry is deleted still
/// refolds and clears its stale projection.
///
/// Reason mask 1 is the content-candidate refold bit; the zero timestamps sort these oldest, so
/// they settle ahead of newly-dirtied streams. The upsert ORs into an existing queue row, and a
/// replay is idempotent by the same shape (the hook is idempotent too — a purged store has
/// nothing left to purge).
///
/// Flagged `ledger_atomic`: the queue rows, the deletions, and the ledger stamp
/// commit together, so an old writer racing the upgrade cannot slip poison into a half-purged
/// store that still answers V112-compatible, and a crash cannot leave a partial purge behind an
/// unstamped ledger. Neither statement below opens a transaction of its own — the ladder owns it.
pub(crate) fn apply_refold_content_streams_for_lamport_clamp(
    conn: &Connection,
    hooks: &crate::hooks::MigrationHooks,
) -> rusqlite::Result<()> {
    conn.execute_batch(
        "INSERT INTO content_streams_pending_refold(
             stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms)
         SELECT DISTINCT stream_id, 1, 0, 0 FROM content_entries WHERE true
         ON CONFLICT(stream_id) DO UPDATE SET
             reason_mask = content_streams_pending_refold.reason_mask | 1;",
    )?;
    (hooks.purge_legacy_lamport_violators)(conn)
}

/// V114 (#1176): denormalize the `/3` header lamport into a `content_entries` column.
///
/// The accepted stream clock (`MAX(lamport)` over a stream's accepted rows) used to be derived by
/// decoding every accepted envelope — O(stream) per read. That was tolerable for the authoring
/// mint's cadence, but the ingest-time bounded-advance gate reads the clock for any authenticated
/// envelope claiming a high lamport, and a hostile roster device re-sending one such envelope
/// bought the full scan under the writer lock every time, with nothing stored for the candidate
/// budgets to throttle. The column plus the partial accepted-rows index make the clock an indexed
/// `MAX`.
///
/// The lamport is part of the signed envelope, so the column is immutable per row and written at
/// every insert site; the hook backfills existing rows by decoding them once. The column stays
/// nullable: a NULL (an undecodable legacy blob) is simply invisible to `MAX`, which is the same
/// treatment the decoding scan gave rows it could not decode. Guarded on the shape so a
/// full-ladder replay over an already-migrated store is a no-op.
pub(crate) fn apply_content_entries_lamport_column(
    conn: &Connection,
    hooks: &crate::hooks::MigrationHooks,
) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "content_entries", "lamport", "INTEGER")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_content_entries_stream_accepted_lamport
             ON content_entries(stream_id, lamport) WHERE accepted = 1;
         -- The refold-persisted lamport clock floor per /3 stream: max over the accepted lamports
         -- AND the in-bound condemned basis, so revoking a writer cannot deflate the clock below
         -- what honest dependents minted against. Invariant: only positive floors are stored
         -- (absence = no clock yet, readers fall back to the accepted-rows MAX); written only by
         -- the acceptance refold. V113 queues every content stream, so the first settle after
         -- this upgrade populates it.
         CREATE TABLE IF NOT EXISTS content_stream_clocks(
             stream_id BLOB PRIMARY KEY CHECK(length(stream_id) = 32),
             clock     INTEGER NOT NULL CHECK(clock > 0)
         ) STRICT;",
    )?;
    (hooks.backfill_content_lamport)(conn)
}

/// V115 (#1178): re-judge every account's persisted authority projection under the fold's
/// grants-require-PublicRead rule.
///
/// The fold change alone only affects accounts that happen to refold after the upgrade; a
/// projected `account_stream_grants` row from a grant a pre-gate binary folded effective on a
/// private stream keeps answering `grant_effective` = Effective until then — leaving exactly the
/// hostile entries the gate targets authorized. The body is intentionally empty: the whole
/// migration IS the ledger-atomic `backfill_authority_projection` hook (the all-account refold),
/// which `apply_and_record_migration` fires for this id the same way it does for V064/V065/V099.
/// The refold also queues affected content streams, so content accepted under a now-rejected
/// grant is re-judged at the next settle.
pub(crate) fn apply_refold_account_authority_projections(
    _conn: &Connection,
) -> rusqlite::Result<()> {
    Ok(())
}

/// V121 (#1282): re-judge persisted verdicts once freshness measures a cited control-log length
/// against the held log instead of the effective count.
///
/// Freshness is re-derived only when a stream or an account refolds. Content an older binary parked
/// `auth_len_ahead` after a cut lowered its author's effective count — and whose memories the drain
/// then removed — would otherwise stay parked until unrelated entries arrived. Queue every stream
/// that holds content for the next settle (the V113 shape: mask 1, zero timestamps so they settle
/// first, ORed into an existing queue row). `apply_and_record_migration` also runs the all-account
/// refold hook for this id, which re-judges secrets-log wraps parked the same way.
pub(crate) fn apply_refold_for_held_control_log_freshness(
    conn: &Connection,
) -> rusqlite::Result<()> {
    conn.execute_batch(
        "INSERT INTO content_streams_pending_refold(
             stream_id, reason_mask, first_enqueued_at_ms, last_enqueued_at_ms)
         SELECT DISTINCT stream_id, 1, 0, 0 FROM content_entries WHERE true
         ON CONFLICT(stream_id) DO UPDATE SET
             reason_mask = content_streams_pending_refold.reason_mask | 1;",
    )
}

/// V125 (#1301): re-judge persisted verdicts once a revoking cut vouches for the ops authored
/// concurrently with it.
///
/// Freshness is re-derived only when an account refolds, and content acceptance only when its
/// stream does. A control op an older binary parked `auth_len_ahead` behind the ops a cut condemned
/// stays parked in the persisted projection, and so does content that cited it, until unrelated
/// entries arrive. `apply_and_record_migration` runs the all-account refold hook for this id; the
/// body queues every content stream for the next settle, as V121 does.
pub(crate) fn apply_refold_for_concurrent_cut_vouch(conn: &Connection) -> rusqlite::Result<()> {
    apply_refold_for_held_control_log_freshness(conn)
}

/// V126 (#1319): `memory_note_summaries`, the summary of a memory's current note keyed
/// `(repo_id, memory_id)` — `content_hash` becomes a synced column. The retired
/// `memory_summaries` keyed the hash so a stale summary self-invalidated, which made every
/// regeneration a delete plus an insert and, on `overlay/1`, a tombstone on every device; every
/// reader already checks the hash against the memory's current note, so the key never carried
/// that guarantee. The old table stays: retained sync entries name it and older binaries write it.
///
/// Seeded from the retired table with each memory's newest row (by `generated_at_ms`, ties by
/// `content_hash`), so no summary has to be regenerated by a model call. A seeded row whose hash
/// is stale is inert — readers reject it and the compaction queue regenerates. Idempotent: the
/// ladder replays on a full index, so the CREATE is `IF NOT EXISTS` and the seed `OR IGNORE`.
/// The seed carries only rows whose repo still exists: repository adoption re-points the live
/// table onto the real id and drops the source `repos` row, but leaves the retired table under
/// the source id, so a replay would otherwise re-create rows there that no repo owns and no purge
/// reaches.
pub fn apply_memory_note_summaries(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_note_summaries(
            repo_id         TEXT    NOT NULL DEFAULT '__unassigned__',
            memory_id       TEXT    NOT NULL,
            -- The hash of the note this summary was generated for. A COLUMN, not part of the key:
            -- every reader compares it (and prompt_version) to the memory's current note, so a
            -- stale summary is rejected without the row changing identity, and a regeneration is
            -- one upsert of the memory's one row — on overlay/1 an Upsert with no Remove (#1319).
            content_hash    TEXT    NOT NULL,
            summary         TEXT    NOT NULL,
            model_id        TEXT,
            prompt_version  TEXT,
            generated_at_ms INTEGER NOT NULL,
            PRIMARY KEY (repo_id, memory_id)
        ) STRICT;
        INSERT OR IGNORE INTO memory_note_summaries(
            repo_id, memory_id, content_hash, summary, model_id, prompt_version, generated_at_ms)
        SELECT s.repo_id, s.memory_id, s.content_hash, s.summary, s.model_id, s.prompt_version,
               s.generated_at_ms
          FROM memory_summaries s
         WHERE s.repo_id IN (SELECT repo_id FROM repos)
           AND NOT EXISTS (
               SELECT 1 FROM memory_summaries newer
                WHERE newer.repo_id = s.repo_id AND newer.memory_id = s.memory_id
                  AND (newer.generated_at_ms > s.generated_at_ms
                       OR (newer.generated_at_ms = s.generated_at_ms
                           AND newer.content_hash > s.content_hash)));",
    )
}

/// V116 (#1179): rebuild `sync_invites` for cross-account WRITER invites.
///
/// A writer invite mints before the grantee account is known — the `StreamGrant` is authored at
/// redemption — so the row carries the target `stream_id` instead of enrollment state, and the
/// used-columns invariant branches by role: an enrollment redemption records the joining device's
/// keys and its receipt, a writer redemption records the dialing transport node and the authored
/// grant id (in `receipt_hash`). Invites are ephemeral (nonce + expiry), so the copy is
/// best-effort continuity, not a compatibility surface.
pub(crate) fn apply_writer_invites(conn: &Connection) -> rusqlite::Result<()> {
    if column_exists(conn, "sync_invites", "stream_id")? {
        return Ok(());
    }
    conn.execute_batch(
        "DROP TABLE IF EXISTS sync_invites_v116;
         CREATE TABLE sync_invites_v116(
             nonce         BLOB    NOT NULL PRIMARY KEY CHECK(length(nonce) = 32),
             account_id    BLOB    NOT NULL CHECK(length(account_id) = 32),
             role          TEXT    NOT NULL CHECK(role IN ('read_only', 'member', 'owner',
                                                           'writer')),
             -- The stream a WRITER invite grants on, resolved at mint (the acceptor serves the
             -- whole store and has no repo scope at redemption). Exactly the writer rows carry
             -- it.
             stream_id     BLOB    CHECK(stream_id IS NULL OR length(stream_id) = 32),
             label         TEXT,
             expires_at_ms INTEGER NOT NULL,
             created_at_ms INTEGER NOT NULL,
             used_at_ms    INTEGER,
             used_transport_node BLOB CHECK(
                 used_transport_node IS NULL OR length(used_transport_node) = 32
             ),
             used_ed25519_pubkey BLOB CHECK(
                 used_ed25519_pubkey IS NULL OR length(used_ed25519_pubkey) = 32
             ),
             used_x25519_pubkey BLOB CHECK(
                 used_x25519_pubkey IS NULL OR length(used_x25519_pubkey) = 32
             ),
             receipt_hash BLOB CHECK(receipt_hash IS NULL OR length(receipt_hash) = 32),
             receipt_signed BLOB,
             receipt_entries BLOB CHECK(
                 receipt_entries IS NULL OR length(receipt_entries) % 32 = 0
             ),
             receipt_bytes BLOB,
             CHECK((role = 'writer') = (stream_id IS NOT NULL)),
             CHECK(
                 (used_at_ms IS NULL
                  AND used_transport_node IS NULL
                  AND used_ed25519_pubkey IS NULL
                  AND used_x25519_pubkey IS NULL
                  AND receipt_hash IS NULL
                  AND receipt_signed IS NULL
                  AND receipt_entries IS NULL
                  AND receipt_bytes IS NULL)
                 OR
                 (role = 'writer'
                  AND used_at_ms IS NOT NULL
                  AND used_transport_node IS NOT NULL
                  AND used_ed25519_pubkey IS NULL
                  AND used_x25519_pubkey IS NULL
                  AND receipt_hash IS NOT NULL
                  AND receipt_signed IS NULL
                  AND receipt_entries IS NULL
                  AND receipt_bytes IS NULL)
                 OR
                 (role != 'writer'
                  AND used_at_ms IS NOT NULL
                  AND used_transport_node IS NOT NULL
                  AND used_ed25519_pubkey IS NOT NULL
                  AND used_x25519_pubkey IS NOT NULL
                  AND receipt_hash IS NOT NULL
                  AND receipt_signed IS NOT NULL
                  AND (receipt_entries IS NOT NULL OR receipt_bytes IS NOT NULL))
              )
         ) STRICT;
         INSERT INTO sync_invites_v116(
             nonce, account_id, role, stream_id, label, expires_at_ms, created_at_ms,
             used_at_ms, used_transport_node, used_ed25519_pubkey, used_x25519_pubkey,
             receipt_hash, receipt_signed, receipt_entries, receipt_bytes)
         SELECT nonce, account_id, role, NULL, label, expires_at_ms, created_at_ms,
             used_at_ms, used_transport_node, used_ed25519_pubkey, used_x25519_pubkey,
             receipt_hash, receipt_signed, receipt_entries, receipt_bytes
         FROM sync_invites;
         DROP TABLE sync_invites;
         ALTER TABLE sync_invites_v116 RENAME TO sync_invites;
         CREATE INDEX IF NOT EXISTS sync_invites_account_expiry
             ON sync_invites(account_id, expires_at_ms);",
    )
}

/// V117 (#1185): the author-leading `/3` index. `account_is_public_kb` runs per inbound
/// connection BEFORE authentication, and its authored-streams evidence ("which streams has this
/// account actually contributed to?") must therefore be an indexed scan — the existing content
/// indexes all lead with `stream_id`.
pub(crate) fn apply_content_author_stream_index(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_content_entries_author_stream
             ON content_entries(author_account_id, stream_id) WHERE accepted = 1;",
    )
}

/// The `/3` projection gains a node's portable anchor set. NULL is a meaningful value here and the
/// reason the column is nullable rather than defaulted to `'[]'`: it means no `node_anchors` op has
/// been folded for the node, which is distinct from an author saying the memory has no bindings.
/// Every existing row starts NULL and stays NULL until the content-projector rebuild re-folds its
/// stream, which the accompanying projector bump forces.
///
/// Guarded on the shape, so a full-ladder replay over a store already provisioned from the
/// end-state snapshot is a no-op rather than a duplicate-column failure.
pub(crate) fn apply_content_projected_node_anchors(conn: &Connection) -> rusqlite::Result<()> {
    ensure_content_projection_shape(conn)
}

/// The `/3` projection gains the source hash its author anchored to. Nullable, and NULL is
/// meaningful: no `node_source_hash` op has been folded for the node, which the drive-by surfaces
/// treat as "no evidence of drift" and leave unmarked — never as evidence that there is none.
///
/// Delegates to [`ensure_content_projection_shape`] for the reason documented there: a projected
/// column has two homes, and the refold steps need the projection's final shape.
pub(crate) fn apply_content_projected_node_source_hash(conn: &Connection) -> rusqlite::Result<()> {
    ensure_content_projection_shape(conn)
}

/// A synced memory records WHICH published anchor set and source hash the drain last applied to it,
/// and what that set named for each symbol anchor it still matched; the `/3` projection records
/// which account authored each node's winning anchor set (#1243). Without the record a receiver can
/// only seed once, so an author's later rebind never reaches it; with it, the drain replaces
/// bindings when the published set CHANGES and leaves them alone otherwise — which keeps a local
/// relocation from being undone on every pass — and tells an author's retarget from a republish of
/// the same target. The author column lets the drain leave a set its own account's `anchors/1`
/// already carries to that carrier.
///
/// Every column is nullable and starts NULL. The projected one delegates to
/// [`ensure_content_projection_shape`] for the reason documented there: a projected column has two
/// homes, and the refold steps need the projection's final shape.
pub fn apply_memory_applied_anchor_snapshot(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "repo_memories", "anchors_applied_digest", "TEXT")?;
    add_column_if_missing(conn, "repo_memories", "source_hash_applied", "TEXT")?;
    add_column_if_missing(conn, "repo_memories", "anchors_applied_targets", "TEXT")?;
    ensure_content_projection_shape(conn)
}

/// A condemned or quarantined synced memory's applied-anchor baseline, parked while its
/// `repo_memories` row is gone (#1298). The row's bindings stay: they replicate on `anchors/1`, so
/// deleting them would publish a `Remove` to every device of the account, including ones where the
/// memory is still live. The baseline is what lets the memory converge on a rebind published while
/// it was away once it returns; it lived on the row, so it is parked here and restored when the
/// drain materializes the memory again, together with the memory's own `source_text_hash`, which
/// the drain stamps only beside bindings that match the published set — a row relocated here would
/// otherwise return without one. The applied source hash is not part of it: a returning row reads
/// that as absent, which is what makes the drain run the stamp again. Keyed and swept by `repo_id`
/// like every repo-scoped table.
pub fn apply_memory_parked_anchor_baselines(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS repo_memory_parked_baselines(
             repo_id TEXT NOT NULL,
             memory_id TEXT NOT NULL,
             anchors_applied_digest TEXT,
             anchors_applied_targets TEXT,
             source_text_hash TEXT,
             PRIMARY KEY (repo_id, memory_id)
         ) STRICT;",
    )
}

/// This store's resolution of each memory binding, beside the authored anchor (#1297). The
/// portable columns of `repo_memory_bindings` — `binding_id`, `path`, the span, `symbol_kind`,
/// `signature_hash`, `moniker_tool_version` — are what the author bound, and replicate on
/// `anchors/1` and in the `/3` anchor set. Validation used to rewrite them in place as the
/// checkout moved, so every relocation replicated, and devices on different checkouts overwrote
/// each other's rows every pass. Relocation now writes these local shadows instead. `resolved`
/// says whether the store has resolved the row at all: once set, the seven shadows ARE its view,
/// NULL included (a target with no signature, a match with no span); unset, readers fall back to
/// the authored columns. Local (never replicated), nullable, additive.
pub fn apply_memory_binding_resolution(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "repo_memory_bindings", "resolved", "INTEGER")?;
    add_column_if_missing(conn, "repo_memory_bindings", "resolved_binding_id", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "resolved_path", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "resolved_start_line", "INTEGER")?;
    add_column_if_missing(conn, "repo_memory_bindings", "resolved_end_line", "INTEGER")?;
    add_column_if_missing(conn, "repo_memory_bindings", "resolved_symbol_kind", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "resolved_signature_hash", "TEXT")?;
    add_column_if_missing(conn, "repo_memory_bindings", "resolved_moniker_tool_version", "TEXT")?;
    // The path readers look a binding up by where this store resolved it, falling back to the
    // authored path; the expression index keeps that lookup off a table scan, as
    // `idx_repo_memory_bindings_path` does for the authored column. The expression must match the
    // readers' text exactly for SQLite to use it.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_repo_memory_bindings_resolved_path
             ON repo_memory_bindings(IIF(resolved, resolved_path, path));",
    )
}

/// Every column the CURRENT content projector writes, applied ahead of any migration that replays
/// the fold.
///
/// A refold migration runs today's projector against a store frozen at that migration's version, so
/// it needs the projection's FINAL shape, not the shape that existed when it was written. Adding a
/// projected-node column therefore has two obligations: the owning migration, and this function.
/// Miss the second and every store older than the earliest refold step fails to open, on a column
/// its own migration body never mentions.
///
/// Each step is guarded on the shape, so calling this before a refold and again from the owning
/// migration is a no-op rather than a duplicate-column failure. The whole body is additionally
/// guarded on the TABLE, because the refold steps start at V064 while the projection itself arrives
/// later in the ladder — a store replaying from before it has nothing to widen yet, and reaches the
/// column through the owning migration on the way to the tip.
pub(crate) fn ensure_content_projection_shape(conn: &Connection) -> rusqlite::Result<()> {
    if !sqlite_object_exists(conn, "table", "content_projected_nodes")? {
        return Ok(());
    }
    add_column_if_missing(conn, "content_projected_nodes", "anchors_json", "TEXT")?;
    add_column_if_missing(conn, "content_projected_nodes", "source_text_hash", "TEXT")?;
    add_column_if_missing(conn, "content_projected_nodes", "anchors_author", "BLOB")?;
    add_column_if_missing(conn, "content_projected_nodes", "superseded_anchors_json", "TEXT")
}

/// V124 (#1304): the anchor sets a node's register held before the winning one, as the content
/// projector writes them. Nullable — NULL until a second publication folds — and populated by the
/// projector's rebuild on the version bump that ships beside this column, not by a backfill here.
pub fn apply_content_projected_superseded_anchors(conn: &Connection) -> rusqlite::Result<()> {
    add_column_if_missing(conn, "content_projected_nodes", "superseded_anchors_json", "TEXT")
}

/// V127 — `sync_tombstone_statements`, the delivery half of a row tombstone (#1295).
///
/// `sync_row_tombstones` holds the latest delete of a row — merge state, permanent, keyed on the
/// row. Which ENTRY carries that delete is a separate question with a separate answer per device
/// chain: retention pins, on every chain that states a current orphan tombstone, the entry at
/// that chain's newest statement. Until now the only statement was the original `Remove`, so the
/// entry that first stated a delete could never be reclaimed; a chain now restates its own
/// deletes at its tail (`Restate`) and this table records where each chain's statement sits.
///
/// Keyed on the tombstone's row plus the stating chain; `repo_id` rides along like the other
/// merge tables so the purge's class-level sweep reaches it. Backfilled with one statement per
/// tombstone at the tombstone's own identity, whether or not that entry still exists: a
/// statement whose entry is already gone is exactly the stranded case compaction's mandatory
/// repair phase carries forward on the local chain, so the backfill preserves that debt rather
/// than hiding it. Idempotent: `IF NOT EXISTS` and `OR IGNORE`, so a replay never lowers a
/// statement a restatement has since advanced.
pub fn apply_tombstone_statements(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_tombstone_statements(
             stream_id          BLOB    NOT NULL CHECK(length(stream_id) = 32),
             repo_id            TEXT    NOT NULL,
             table_name         TEXT    NOT NULL,
             row_pk             TEXT    NOT NULL,
             -- The stating chain, in the lowercase hex the merge tables use.
             device_fingerprint TEXT    NOT NULL,
             -- The lamport of that chain's newest entry stating the row's current tombstone.
             lamport            INTEGER NOT NULL,
             PRIMARY KEY(stream_id, table_name, row_pk, device_fingerprint)
         ) STRICT;
         -- Retention reads a chain's statements in lamport order (`chain_pins`).
         CREATE INDEX IF NOT EXISTS sync_tombstone_statements_chain
             ON sync_tombstone_statements(stream_id, device_fingerprint, lamport);
         INSERT OR IGNORE INTO sync_tombstone_statements(
             stream_id, repo_id, table_name, row_pk, device_fingerprint, lamport)
         SELECT stream_id, repo_id, table_name, row_pk, device_fingerprint, lamport
           FROM sync_row_tombstones;",
    )
}
