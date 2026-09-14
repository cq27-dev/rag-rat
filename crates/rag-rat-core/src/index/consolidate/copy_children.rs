use std::collections::BTreeMap;

use rusqlite::{Connection, params};

use super::import::ImportMode;
use crate::index::schema;

/// Copy `repo_memory_bindings`, stamping `repo_id` and NULLing ONLY the LOCAL rowid columns
/// (`logical_symbol_id` / `symbol_id` / `chunk_id` / `edge_id`) so the validate loop re-resolves
/// them from the portable anchor after the next index pass (spec §4.5). EVERY portable column is
/// copied verbatim — including the relocation-provenance set (`symbol_kind`, `signature_hash`,
/// `moniker_tool`, `moniker_tool_version`, `relocation_reason`): moniker validation reports
/// `unverified` without `moniker_tool`, and moniker relocation requires both tool fields, so
/// dropping them would permanently strip imported `scip_moniker` bindings of their oracle-backed
/// relocation path. The column probes tolerate a legacy DB predating the signals /
/// moniker-provenance migrations (absent columns import as NULL).
pub(super) fn copy_bindings(
    source: &Connection,
    tx: &Connection,
    repo_id: &str,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_memory_bindings")? {
        return Ok(0);
    }
    let symbol_kind = source_column_or_null(source, "symbol_kind")?;
    let signature_hash = source_column_or_null(source, "signature_hash")?;
    let moniker_tool = source_column_or_null(source, "moniker_tool")?;
    let moniker_tool_version = source_column_or_null(source, "moniker_tool_version")?;
    let relocation_reason = source_column_or_null(source, "relocation_reason")?;
    // This store's resolution of the anchor (#1297): the import takes the source's local
    // call-path tables wholesale, keyed by the hash the source resolved, so it takes the
    // resolution with them — all of it, since a resolved row's shadows are one view. A source
    // from before the columns contributes NULLs, i.e. no resolution beyond the authored one.
    let resolution: Vec<String> = [
        "resolved",
        "resolved_binding_id",
        "resolved_path",
        "resolved_start_line",
        "resolved_end_line",
        "resolved_symbol_kind",
        "resolved_signature_hash",
        "resolved_moniker_tool_version",
    ]
    .iter()
    .map(|column| source_column_or_null(source, column))
    .collect::<anyhow::Result<_>>()?;
    let resolution = resolution.join(", ");
    // The tracker columns exist per source VINTAGE: a post-V060 source carries
    // tracker/project/item_key, a pre-V060 source carries github_owner/github_repo/github_number
    // — probe both shapes and convert legacy `github` bindings to the `tracker` kind below (the
    // V060 mapping, applied at the import seam because a foreign source file is read as-is,
    // never migrated).
    let tracker_col = source_column_or_null(source, "tracker")?;
    let project_col = source_column_or_null(source, "project")?;
    let item_key_col = source_column_or_null(source, "item_key")?;
    let github_owner = source_column_or_null(source, "github_owner")?;
    let github_repo = source_column_or_null(source, "github_repo")?;
    let github_number = source_column_or_null(source, "github_number")?;
    let mut stmt = source.prepare(&format!(
        "SELECT memory_id, binding_kind, binding_id, path, start_line, end_line, commit_hash, \
         {tracker_col}, {project_col}, {item_key_col}, {github_owner}, {github_repo}, \
         {github_number}, anchor_status, created_at_ms, {symbol_kind}, {signature_hash}, \
         {moniker_tool}, {moniker_tool_version}, {relocation_reason}, {resolution}
         FROM repo_memory_bindings",
    ))?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        // Only rows whose parent memory this import OWNS (the id map); an unmapped memory_id is a
        // dangling orphan in the source — dropped, never attached to a stranger's memory.
        let Some(memory_id) = id_map.get(&row.get::<_, String>(0)?) else {
            continue;
        };
        let mut binding_kind = row.get::<_, String>(1)?;
        let mut binding_id = row.get::<_, String>(2)?;
        let mut tracker = row.get::<_, Option<String>>(7)?;
        let mut project = row.get::<_, Option<String>>(8)?;
        let mut item_key = row.get::<_, Option<String>>(9)?;
        // Legacy `github` bindings convert to the `tracker` kind — exactly the V060 backfill
        // mapping, so an imported binding is indistinguishable from a migrated one.
        if binding_kind == "github"
            && let (Some(owner), Some(gh_repo), Some(number)) = (
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<i64>>(12)?,
            )
        {
            binding_kind = "tracker".to_string();
            binding_id = format!("github:{owner}/{gh_repo}#{number}");
            tracker = Some("github".to_string());
            project = Some(format!("{owner}/{gh_repo}"));
            item_key = Some(number.to_string());
        }
        let changed = tx.execute(
            "INSERT OR IGNORE INTO repo_memory_bindings(memory_id, binding_kind, binding_id, \
             path, start_line, end_line, logical_symbol_id, symbol_id, chunk_id, edge_id, \
             commit_hash, tracker, project, item_key, anchor_status, created_at_ms, symbol_kind, \
             signature_hash, moniker_tool, moniker_tool_version, relocation_reason, repo_id, \
             resolved, resolved_binding_id, resolved_path, resolved_start_line, \
             resolved_end_line, resolved_symbol_kind, resolved_signature_hash, \
             resolved_moniker_tool_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, NULL, ?7, ?8, ?9, ?10, ?11, ?12, \
             ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
            params![
                memory_id,
                binding_kind,
                binding_id,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
                tracker,
                project,
                item_key,
                row.get::<_, String>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, Option<String>>(15)?,
                row.get::<_, Option<String>>(16)?,
                row.get::<_, Option<String>>(17)?,
                row.get::<_, Option<String>>(18)?,
                row.get::<_, Option<String>>(19)?,
                repo_id,
                row.get::<_, Option<i64>>(20)?,
                row.get::<_, Option<String>>(21)?,
                row.get::<_, Option<String>>(22)?,
                row.get::<_, Option<i64>>(23)?,
                row.get::<_, Option<i64>>(24)?,
                row.get::<_, Option<String>>(25)?,
                row.get::<_, Option<String>>(26)?,
                row.get::<_, Option<String>>(27)?,
            ],
        )?;
        count += changed as u64;
    }
    Ok(count)
}

/// `column` when the SOURCE `repo_memory_bindings` carries it, else a `NULL AS column` literal —
/// the probe that lets [`copy_bindings`] read one SELECT shape from any legacy vintage. `column`
/// is a compile-time constant at every call site, never user input.
fn source_column_or_null(source: &Connection, column: &str) -> anyhow::Result<String> {
    Ok(if schema::column_exists(source, "repo_memory_bindings", column)? {
        column.to_string()
    } else {
        format!("NULL AS {column}")
    })
}

/// Copy `repo_memory_tags` (scoped transitively via `memory_id` — no `repo_id` column; both
/// columns copied, the full table shape).
/// Copy `repo_node_edges` (#464), stamping the OWNER `repo_id` and REMAPPING both endpoints through
/// the id map. An edge's SOURCE must map — an edge of an unmapped memory is a dangling orphan in
/// the source, dropped, never attached to a stranger (the child-ownership invariant). A NODE target
/// that ALSO maps is remapped (id + repo) and `current`; a node target that does NOT map is kept
/// verbatim as an `unresolved` cross-repo reference; a github target re-homes to the import repo
/// and stays `current`. The `edge_key` is RECOMPUTED from the remapped coordinates — it
/// content-addresses owner+source+target, all of which change on import. Local rowid columns are
/// NOT copied (re-resolved on read); `INSERT OR IGNORE` because `refresh_children` cleared the
/// source's edge set this run.
pub(super) fn copy_node_edges(
    source: &Connection,
    tx: &Connection,
    repo_id: &str,
    id_map: &BTreeMap<String, String>,
    mode: ImportMode,
    own: &rag_rat_oplog::CreatedContent,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_node_edges")? {
        return Ok(0);
    }
    // Edges carry their OWN `origin`: another account's edge onto one of our local memories is
    // `synced` yet its source node is in `id_map`, so it is dropped here or it would be re-authored
    // as our own `EdgeAdd`. A synced edge the source's own account added is carried like a local
    // one (`own`, empty for seed). (Repo scope rides `id_map`, already filtered by
    // `copy_memories`.)
    let mut stmt = source.prepare(
        "SELECT source_node_id, relation, target_repo_id, target_kind, target_anchor, \
         created_at_ms, repo_id FROM repo_node_edges
         WHERE origin = 'local' OR edge_key IN (SELECT value FROM json_each(?1))",
    )?;
    let mut rows = stmt.query(params![serde_json::to_string(&own.edges)?])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        // Child-ownership: only edges whose SOURCE this import owns; an unmapped source is dropped.
        let Some(source_node_id) = id_map.get(&row.get::<_, String>(0)?) else {
            continue;
        };
        let relation = row.get::<_, String>(1)?;
        let src_target_repo = row.get::<_, String>(2)?;
        let target_kind = row.get::<_, String>(3)?;
        let src_target_anchor = row.get::<_, String>(4)?;
        let created_at_ms = row.get::<_, i64>(5)?;
        let src_repo = row.get::<_, String>(6)?;
        let (target_repo_id, target_anchor, target_node_id, anchor_status) =
            match target_kind.as_str() {
                "node" => match id_map.get(&src_target_anchor) {
                    Some(mapped) =>
                        (repo_id.to_string(), mapped.clone(), Some(mapped.clone()), "current"),
                    // A node target outside the imported set. `add_edge` allows explicit cross-repo
                    // node edges, so on a MULTI-repo seed source this points at a DIFFERENT private
                    // repo (id_map holds only the published repo) — carrying its repo_id + node id
                    // would leak that repo onto the public op-log once the edge is authored. Drop
                    // it. Legacy consolidation KEEPS it: either an explicit cross-repo reference,
                    // or a same-repo memory the import left out because another
                    // account created it, which may come back through sync — so
                    // a same-repo target is re-homed under the new identity,
                    // where `resolve_node_target` finds the row if it ever arrives.
                    None if matches!(mode, ImportMode::SeedPublic) => continue,
                    None if src_target_repo == src_repo =>
                        (repo_id.to_string(), src_target_anchor.clone(), None, "unresolved"),
                    None => (src_target_repo, src_target_anchor.clone(), None, "unresolved"),
                },
                _ => (repo_id.to_string(), src_target_anchor.clone(), None, "current"),
            };
        let key = rag_rat_query::memory::edge_key(
            source_node_id,
            &relation,
            &target_kind,
            &target_anchor,
        );
        let changed = tx.execute(
            "INSERT OR IGNORE INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, \
             target_repo_id, target_kind, target_anchor, target_node_id, \
             target_logical_symbol_id, symbol_kind, signature_hash, anchor_status, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL, ?9, ?10)",
            params![
                key,
                repo_id,
                source_node_id,
                relation,
                target_repo_id,
                target_kind,
                target_anchor,
                target_node_id,
                anchor_status,
                created_at_ms
            ],
        )?;
        count += changed as u64;
    }
    Ok(count)
}

pub(super) fn copy_tags(
    source: &Connection,
    tx: &Connection,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_memory_tags")? {
        return Ok(0);
    }
    let mut stmt = source.prepare("SELECT memory_id, tag FROM repo_memory_tags")?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let Some(memory_id) = id_map.get(&row.get::<_, String>(0)?) else {
            continue;
        };
        let changed = tx.execute(
            "INSERT OR IGNORE INTO repo_memory_tags(memory_id, tag) VALUES (?1, ?2)",
            params![memory_id, row.get::<_, String>(1)?],
        )?;
        count += changed as u64;
    }
    Ok(count)
}

/// Copy `repo_memory_call_paths`, NULLing the local `start`/`end_logical_symbol_id` (re-resolved
/// by the validate loop, like the bindings' rowid columns). The path identity is copied first and
/// then re-keyed by [`rag_rat_query::memory::remap_call_path_callee_logical_symbol_ids`] when a
/// callee id changes.
pub(super) fn copy_call_paths(
    source: &Connection,
    tx: &Connection,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_memory_call_paths")? {
        return Ok(0);
    }
    let mut stmt = source.prepare(
        "SELECT memory_id, edge_sequence_hash, path_summary, created_at_ms
         FROM repo_memory_call_paths",
    )?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let Some(memory_id) = id_map.get(&row.get::<_, String>(0)?) else {
            continue;
        };
        let changed = tx.execute(
            "INSERT OR IGNORE INTO repo_memory_call_paths(memory_id, start_logical_symbol_id, \
             end_logical_symbol_id, edge_sequence_hash, path_summary, created_at_ms)
             VALUES (?1, NULL, NULL, ?2, ?3, ?4)",
            params![
                memory_id,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ],
        )?;
        count += changed as u64;
    }
    Ok(count)
}

/// Copy `repo_memory_call_path_edges`. Pre-V099 sources have no callee identity columns; copy them
/// as unknown so validation fails closed until an exact compatibility match converges the row.
/// Current rows are copied first, then the import transaction re-derives their callee ids,
/// fingerprints, sequence hashes, and binding ids under the destination repo identity.
pub(super) fn copy_call_path_edges(
    source: &Connection,
    tx: &Connection,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_memory_call_path_edges")? {
        return Ok(0);
    }
    let callee_columns =
        if schema::column_exists(source, "repo_memory_call_path_edges", "callee_identity_known")? {
            "callee_logical_symbol_id, callee_identity_known"
        } else {
            "NULL, 0"
        };
    let mut stmt = source.prepare(&format!(
        "SELECT memory_id, edge_sequence_hash, ordinal, edge_fingerprint, from_name, to_name, \
         edge_kind, target_qualified_name, receiver_hint, {callee_columns} FROM \
         repo_memory_call_path_edges"
    ))?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let Some(memory_id) = id_map.get(&row.get::<_, String>(0)?) else {
            continue;
        };
        let changed = tx.execute(
            "INSERT OR IGNORE INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, \
             ordinal, edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name, \
             receiver_hint, callee_logical_symbol_id, callee_identity_known)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                memory_id,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, i64>(10)?,
            ],
        )?;
        count += changed as u64;
    }
    Ok(count)
}
