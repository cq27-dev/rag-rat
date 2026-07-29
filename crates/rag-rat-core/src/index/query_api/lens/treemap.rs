//! Batched per-file metrics for the editor hotspot treemap.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::{Connection, params};
use serde::Serialize;

use super::super::clones::precompute::{build_anchor_index, live_generation_row};
use super::clones::{CloneFileMetrics, record_clone_metric};
use crate::index::IndexDatabase;

#[derive(Clone, Debug, Serialize)]
pub struct LensTreemap {
    pub files: Vec<LensTreemapFile>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LensTreemapFile {
    pub path: String,
    pub language: String,
    pub kind: String,
    pub loc: i64,
    pub churn_commits: i64,
    pub churn_lines: i64,
    pub fan_in: i64,
    pub fan_out: i64,
    pub dup_partners: i64,
    pub dup_max_similarity: f64,
    pub memories: i64,
}

impl IndexDatabase {
    pub fn lens_treemap(&self) -> anyhow::Result<LensTreemap> {
        let cancelled = AtomicBool::new(false);
        self.lens_treemap_with_cancel(&cancelled)
    }

    pub fn lens_treemap_with_cancel(&self, cancelled: &AtomicBool) -> anyhow::Result<LensTreemap> {
        ensure_not_cancelled(cancelled)?;
        let conn = self.storage.connection();
        let clone_metrics = if self.active_scope_is_linked_overlay() {
            self.lens_overlay_clone_metrics(cancelled)?
        } else {
            let clone_generation = live_generation_row(conn)?
                .filter(|row| row.normalizer_version == rag_rat_clones::NORM_VERSION)
                .map(|row| row.generation)
                .unwrap_or(-1);
            current_clone_metrics(conn, clone_generation, cancelled)?
        };
        ensure_not_cancelled(cancelled)?;
        let mut stmt = conn.prepare(
            "WITH
             loc AS (
                 SELECT chunk.file_id, MAX(chunk.end_line) AS lines
                 FROM chunks chunk
                 JOIN files file ON file.id = chunk.file_id
                 GROUP BY chunk.file_id
             ),
             churn AS (
                 SELECT path, COUNT(*) AS commits,
                        SUM(COALESCE(additions, 0) + COALESCE(deletions, 0)) AS lines
                 FROM git_file_changes
                 WHERE repo_id = ?1
                 GROUP BY path
             ),
             incoming AS (
                 SELECT target_file.id AS file_id, COUNT(*) AS count
                 FROM edges edge
                 JOIN files source_file ON source_file.id = edge.source_file_id
                 JOIN symbols target_symbol ON target_symbol.id = edge.to_symbol_id
                 JOIN files target_file ON target_file.id = target_symbol.file_id
                 WHERE source_file.id != target_file.id
                 GROUP BY target_file.id
             ),
             outgoing AS (
                 SELECT source_file.id AS file_id, COUNT(*) AS count
                 FROM edges edge
                 JOIN files source_file ON source_file.id = edge.source_file_id
                 JOIN symbols target_symbol ON target_symbol.id = edge.to_symbol_id
                 JOIN files target_file ON target_file.id = target_symbol.file_id
                 WHERE source_file.id != target_file.id
                 GROUP BY source_file.id
             ),
             active_bindings AS MATERIALIZED (
                 SELECT binding.memory_id, binding.binding_kind, binding.path,
                        binding.symbol_id, binding.chunk_id, binding.logical_symbol_id
                 FROM repo_memories memory
                 JOIN repo_memory_bindings binding
                   ON binding.memory_id = memory.id AND binding.repo_id = memory.repo_id
                 WHERE memory.repo_id = ?1 AND memory.status = 'active'
             ),
             memory_files AS (
                 SELECT binding.memory_id, file.id AS file_id
                 FROM active_bindings binding
                 JOIN files file ON file.path = binding.path
                 UNION
                 SELECT binding.memory_id, file.id
                 FROM active_bindings binding
                 JOIN files file
                   ON binding.binding_kind = 'dir' AND (
                       binding.path = ''
                       OR substr(file.path, 1, length(binding.path) + 1) = binding.path || '/'
                   )
                 UNION
                 SELECT binding.memory_id, file.id
                 FROM active_bindings binding
                 JOIN symbols symbol ON symbol.id = binding.symbol_id
                 JOIN files file ON file.id = symbol.file_id
                 UNION
                 SELECT binding.memory_id, file.id
                 FROM active_bindings binding
                 JOIN chunks chunk ON chunk.id = binding.chunk_id
                 JOIN files file ON file.id = chunk.file_id
                 UNION
                 SELECT binding.memory_id, file.id
                 FROM active_bindings binding
                 JOIN logical_symbol_members member
                   ON member.logical_symbol_id = binding.logical_symbol_id
                 JOIN symbols symbol ON symbol.id = member.symbol_id
                 JOIN files file ON file.id = symbol.file_id
             ),
             memories AS (
                 SELECT file_id, COUNT(*) AS count
                 FROM memory_files
                 GROUP BY file_id
             )
             SELECT file.path, file.language, file.kind,
                     COALESCE(loc.lines, 0),
                     COALESCE(churn.commits, 0), COALESCE(churn.lines, 0),
                     COALESCE(incoming.count, 0), COALESCE(outgoing.count, 0),
                     COALESCE(memories.count, 0)
             FROM files file
             LEFT JOIN loc ON loc.file_id = file.id
             LEFT JOIN churn ON churn.path = file.path
             LEFT JOIN incoming ON incoming.file_id = file.id
             LEFT JOIN outgoing ON outgoing.file_id = file.id
             LEFT JOIN memories ON memories.file_id = file.id
             ORDER BY file.path, file.id",
        )?;
        let rows = stmt.query_map(params![self.active_repo_id], |row| {
            let path: String = row.get(0)?;
            let clones = clone_metrics.get(&path);
            Ok(LensTreemapFile {
                path,
                language: row.get(1)?,
                kind: row.get(2)?,
                loc: row.get(3)?,
                churn_commits: row.get(4)?,
                churn_lines: row.get(5)?,
                fan_in: row.get(6)?,
                fan_out: row.get(7)?,
                dup_partners: clones.map_or(0, |metric| metric.partners.len() as i64),
                dup_max_similarity: clones.map_or(0.0, |metric| metric.max_similarity),
                memories: row.get(8)?,
            })
        })?;
        let files = rows.collect::<rusqlite::Result<_>>()?;
        ensure_not_cancelled(cancelled)?;
        Ok(LensTreemap { files })
    }
}

fn current_clone_metrics(
    conn: &Connection,
    generation: i64,
    cancelled: &AtomicBool,
) -> anyhow::Result<HashMap<String, CloneFileMetrics>> {
    ensure_not_cancelled(cancelled)?;
    if generation < 0 {
        return Ok(HashMap::new());
    }
    let anchors = build_anchor_index(conn)?;
    let mut stmt = conn.prepare(
        "SELECT a_path, a_start_byte, a_file_sha, b_path, b_start_byte, b_file_sha, similarity
         FROM clone_edges WHERE build_generation = ?1",
    )?;
    let rows = stmt.query_map([generation], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, f64>(6)?,
        ))
    })?;
    let mut metrics: HashMap<String, CloneFileMetrics> = HashMap::new();
    for (index, row) in rows.enumerate() {
        if index.is_multiple_of(1024) {
            ensure_not_cancelled(cancelled)?;
        }
        let (a_path, a_start, a_sha, b_path, b_start, b_sha, similarity) = row?;
        let a_anchor = (a_path, a_start);
        let b_anchor = (b_path, b_start);
        let Some((_, live_a_sha)) = anchors.get(&a_anchor) else { continue };
        let Some((_, live_b_sha)) = anchors.get(&b_anchor) else { continue };
        if live_a_sha != &a_sha || live_b_sha != &b_sha {
            continue;
        }
        record_clone_metric(&mut metrics, &a_anchor.0, &b_anchor.0, similarity);
        record_clone_metric(&mut metrics, &b_anchor.0, &a_anchor.0, similarity);
    }
    Ok(metrics)
}

fn ensure_not_cancelled(cancelled: &AtomicBool) -> anyhow::Result<()> {
    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "lens request cancelled");
    Ok(())
}
