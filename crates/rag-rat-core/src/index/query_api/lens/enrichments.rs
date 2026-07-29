//! File-scoped memory and change-coupling compositions.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::Serialize;

use crate::index::IndexDatabase;

const COUPLING_LIMIT: u32 = 10;
pub(super) const MEMORY_LIMIT: usize = 50;
const PAPERTRAIL_LIMIT: u32 = 50;

#[derive(Debug, Serialize)]
pub struct LensFileCoupling {
    pub coupling: Vec<LensCouplingPartner>,
}

#[derive(Debug, Serialize)]
pub struct LensCouplingPartner {
    pub path: String,
    pub co_changes: i64,
    pub my_changes: i64,
    pub confidence: f64,
    pub last_co_change_at_s: i64,
}

#[derive(Debug, Serialize)]
pub struct LensFileMemories {
    pub memories: Vec<LensFileMemory>,
}

#[derive(Debug, Serialize)]
pub struct LensFileMemory {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub confidence: String,
    pub binding_kind: String,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub anchor_status: String,
    pub summary: Option<String>,
    pub verdict: Option<String>,
    pub verdict_direction: Option<String>,
    pub verdict_evidence: Option<String>,
    pub checked_against_commit: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LensFilePapertrail {
    pub refs: Vec<LensPapertrailRef>,
    pub decisions: Vec<LensDecisionRecord>,
}

#[derive(Debug, Serialize)]
pub struct LensPapertrailRef {
    pub tracker: String,
    pub project: String,
    pub item_key: String,
    pub item_kind: String,
    pub ref_kind: String,
    pub source_kind: String,
    pub source_text: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub state_normalized: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LensDecisionRecord {
    pub tracker: String,
    pub project: String,
    pub item_kind: String,
    pub item_key: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub root_issue: Option<String>,
    pub root_cause: Option<String>,
    pub decision_chosen: Option<String>,
    pub outcome_summary: Option<String>,
    pub outcome_status_model: String,
    pub outcome_claim_verified: i64,
    pub decision_provenance_verified: i64,
    pub fix_edge_source: String,
    pub line: Option<i64>,
}

struct ItemDisplay {
    item_kind: String,
    title: Option<String>,
    url: Option<String>,
    state_normalized: Option<String>,
}

#[derive(Debug)]
struct MemoryRow {
    id: String,
    kind: String,
    title: String,
    body: String,
    confidence: String,
    binding_kind: String,
    path: Option<String>,
    line: Option<i64>,
    anchor_status: String,
}

impl IndexDatabase {
    /// Materialize the history-derived coupling cache before a read-only Lens server starts.
    /// Current caches take only the stamp fast path; upgraded indexes pay the bounded rebuild once.
    pub fn materialize_lens_coupling(&self) -> anyhow::Result<()> {
        self.ensure_coupling_fresh()
    }

    pub fn lens_file_coupling(&self, path: &str) -> anyhow::Result<LensFileCoupling> {
        let coupling = crate::index::change_coupling::current_coupled_files_for_path(
            self.storage.connection(),
            &self.active_repo_id,
            path,
            COUPLING_LIMIT,
        )?
        .into_iter()
        .map(|partner| LensCouplingPartner {
            path: partner.other_path,
            co_changes: partner.co_change_count,
            my_changes: partner.this_change_count,
            confidence: (partner.confidence * 1000.0).round() / 1000.0,
            last_co_change_at_s: partner.last_co_change_at_s,
        })
        .collect();
        Ok(LensFileCoupling { coupling })
    }

    pub fn lens_file_memories(&self, path: &str) -> anyhow::Result<LensFileMemories> {
        let conn = self.storage.connection();
        let rows = list_file_memory_rows(conn, &self.active_repo_id, path)?;
        let mut dream_states = batch_file_memory_dream_states(conn, &rows)?;
        let mut memories = Vec::with_capacity(rows.len());
        for row in rows {
            let dream = dream_states.remove(&row.id).unwrap_or_default();
            memories.push(LensFileMemory {
                id: row.id,
                kind: row.kind,
                title: row.title,
                body: row.body,
                confidence: row.confidence,
                binding_kind: row.binding_kind,
                path: row.path,
                line: row.line,
                anchor_status: row.anchor_status,
                summary: dream.summary,
                verdict: dream.verdict,
                verdict_direction: dream.direction,
                verdict_evidence: dream.evidence_json,
                checked_against_commit: dream.checked_against_commit,
            });
        }
        Ok(LensFileMemories { memories })
    }

    pub fn lens_file_papertrail(&self, path: &str) -> anyhow::Result<LensFilePapertrail> {
        let conn = self.storage.connection();
        let mut refs = Vec::new();
        for reference in
            list_unique_papertrail_refs(conn, &self.active_repo_id, path, PAPERTRAIL_LIMIT)?
        {
            let display = item_display(
                conn,
                &self.active_repo_id,
                &reference.tracker,
                &reference.project,
                &reference.item_key,
                Some(&reference.item_kind),
            )?;
            let url = display.url.or_else(|| {
                (reference.tracker == "github").then(|| {
                    format!(
                        "https://github.com/{}/issues/{}",
                        reference.project, reference.item_key
                    )
                })
            });
            refs.push(LensPapertrailRef {
                tracker: reference.tracker,
                project: reference.project,
                item_key: reference.item_key,
                item_kind: display.item_kind,
                ref_kind: reference.ref_kind,
                source_kind: reference.source_kind,
                source_text: reference.source_text,
                title: display.title,
                url,
                state_normalized: display.state_normalized,
            });
        }

        let mut decisions = Vec::new();
        for anchored in rag_rat_papertrail::records_for_path(
            conn,
            &self.active_repo_id,
            path,
            PAPERTRAIL_LIMIT as usize,
        )? {
            let record = anchored.record;
            let display = item_display(
                conn,
                &self.active_repo_id,
                &record.tracker,
                &record.project,
                &record.item_key,
                Some(&record.item_kind),
            )?;
            decisions.push(LensDecisionRecord {
                tracker: record.tracker,
                project: record.project,
                item_kind: record.item_kind,
                item_key: record.item_key,
                title: display.title,
                url: display.url,
                root_issue: record.root_issue,
                root_cause: record.root_cause,
                decision_chosen: record.decision_chosen,
                outcome_summary: record.outcome_summary,
                // Keep the endpoint contract's field name while surfacing the mechanically
                // resolved status.
                outcome_status_model: record.outcome_status.as_db_str().to_string(),
                outcome_claim_verified: i64::from(record.outcome_claim_verified),
                decision_provenance_verified: i64::from(record.decision_provenance_verified),
                fix_edge_source: record.fix_edge_source.as_db_str().to_string(),
                line: anchored.line,
            });
        }
        Ok(LensFilePapertrail { refs, decisions })
    }
}

struct UniquePapertrailRef {
    tracker: String,
    project: String,
    item_key: String,
    item_kind: String,
    ref_kind: String,
    source_kind: String,
    source_text: String,
}

/// Resolve nullable provider kinds, collapse repeated textual references by full item identity,
/// then apply the editor payload cap. Limiting `papertrail_refs` first can let one noisy item evict
/// every lower-ranked unique item.
fn list_unique_papertrail_refs(
    conn: &Connection,
    repo_id: &str,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<UniquePapertrailRef>> {
    let mut stmt = conn.prepare(
        "WITH normalized AS MATERIALIZED (
             SELECT ref.id, ref.tracker, ref.project, ref.item_key,
                    COALESCE(
                        ref.item_kind,
                        (SELECT item.item_kind
                         FROM papertrail_items item
                         WHERE item.repo_id = ref.repo_id AND item.tracker = ref.tracker
                           AND item.project = ref.project AND item.item_key = ref.item_key
                         ORDER BY CASE item.item_kind WHEN 'issue' THEN 0 ELSE 1 END
                         LIMIT 1),
                        'issue'
                    ) AS item_kind,
                    ref.ref_kind, ref.source_kind, ref.source_text, ref.discovered_at_ms
             FROM papertrail_refs ref
             WHERE ref.repo_id = ?1 AND ref.source_path = ?2
         ),
         ranked AS (
             SELECT *, ROW_NUMBER() OVER (
                 PARTITION BY tracker, project, item_kind, item_key
                 ORDER BY discovered_at_ms DESC, id DESC
             ) AS identity_rank
             FROM normalized
         )
         SELECT tracker, project, item_key, item_kind, ref_kind, source_kind, source_text
         FROM ranked
         WHERE identity_rank = 1
         ORDER BY discovered_at_ms DESC, id DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![repo_id, path, i64::from(limit)], |row| {
        Ok(UniquePapertrailRef {
            tracker: row.get(0)?,
            project: row.get(1)?,
            item_key: row.get(2)?,
            item_kind: row.get(3)?,
            ref_kind: row.get(4)?,
            source_kind: row.get(5)?,
            source_text: row.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn item_display(
    conn: &Connection,
    repo_id: &str,
    tracker: &str,
    project: &str,
    item_key: &str,
    explicit_kind: Option<&str>,
) -> anyhow::Result<ItemDisplay> {
    let row = conn
        .query_row(
            "SELECT item_kind, title, url, state_normalized
             FROM papertrail_items
             WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_key = ?4
               AND (?5 IS NULL OR item_kind = ?5)
             ORDER BY CASE item_kind WHEN 'issue' THEN 0 ELSE 1 END
             LIMIT 1",
            params![repo_id, tracker, project, item_key, explicit_kind],
            |row| {
                Ok(ItemDisplay {
                    item_kind: row.get(0)?,
                    title: row.get(1)?,
                    url: row.get(2)?,
                    state_normalized: row.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(row.unwrap_or_else(|| ItemDisplay {
        item_kind: explicit_kind.unwrap_or("issue").to_string(),
        title: None,
        url: None,
        state_normalized: None,
    }))
}

fn list_file_memory_rows(
    conn: &Connection,
    repo_id: &str,
    path: &str,
) -> anyhow::Result<Vec<MemoryRow>> {
    let ancestors = directory_ancestors(path);
    let marks = ancestors.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "WITH requested AS MATERIALIZED (
             SELECT id FROM files WHERE path = ?1
         ),
         matching AS (
             SELECT m.id, m.kind, m.title, m.body, m.confidence,
                    b.binding_kind, b.path, b.anchor_status,
                    COALESCE(
                        b.start_line,
                        direct_symbol.start_line,
                        (SELECT MIN(member.start_line)
                         FROM logical_symbol_members lsm
                         JOIN symbols member ON member.id = lsm.symbol_id
                         WHERE lsm.logical_symbol_id = b.logical_symbol_id
                           AND member.file_id IN (SELECT id FROM requested)),
                        bound_chunk.start_line
                    ) AS line,
                     b.created_at_ms,
                     CASE WHEN b.binding_kind = 'dir' THEN 1 ELSE 0 END AS binding_priority
             FROM repo_memories m
             JOIN repo_memory_bindings b ON b.memory_id = m.id AND b.repo_id = m.repo_id
             LEFT JOIN symbols direct_symbol
               ON direct_symbol.id = b.symbol_id
              AND direct_symbol.file_id IN (SELECT id FROM requested)
             LEFT JOIN chunks bound_chunk
               ON bound_chunk.id = b.chunk_id
              AND bound_chunk.file_id IN (SELECT id FROM requested)
             WHERE m.repo_id = ?2 AND m.status = 'active'
               AND EXISTS (SELECT 1 FROM requested)
               AND (
                   b.path = ?1
                   OR (b.binding_kind = 'dir' AND b.path IN ({marks}))
                   OR direct_symbol.id IS NOT NULL
                   OR bound_chunk.id IS NOT NULL
                   OR EXISTS (
                       SELECT 1
                       FROM logical_symbol_members lsm
                       JOIN symbols member ON member.id = lsm.symbol_id
                       WHERE lsm.logical_symbol_id = b.logical_symbol_id
                         AND member.file_id IN (SELECT id FROM requested)
                   )
               )
         ),
         ranked AS (
              SELECT *, ROW_NUMBER() OVER (
                  PARTITION BY id
                  ORDER BY binding_priority, line IS NULL, line, created_at_ms,
                           binding_kind, COALESCE(path, '')
              ) AS rank
              FROM matching
          )
          SELECT id, kind, title, body, confidence, binding_kind, path, line, anchor_status
          FROM ranked WHERE rank = 1
          ORDER BY binding_priority, line IS NULL, line, id
          LIMIT {MEMORY_LIMIT}"
    );
    let mut values = vec![path.to_string(), repo_id.to_string()];
    values.extend(ancestors);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| {
        Ok(MemoryRow {
            id: row.get(0)?,
            kind: row.get(1)?,
            title: row.get(2)?,
            body: row.get(3)?,
            confidence: row.get(4)?,
            binding_kind: row.get(5)?,
            path: row.get(6)?,
            line: row.get(7)?,
            anchor_status: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Hydrate the bounded editor payload with two set reads instead of two queries per memory.
/// Evidence freshness remains exact, but only memories carrying a current reality row pay that
/// check (and the caller caps the population at [`MEMORY_LIMIT`]).
fn batch_file_memory_dream_states(
    conn: &Connection,
    rows: &[MemoryRow],
) -> rusqlite::Result<HashMap<String, rag_rat_query::memory::CurrentDreamState>> {
    if rows.is_empty() {
        return Ok(HashMap::new());
    }
    let scope = rag_rat_db::schema::periphery_repo_scope(conn, "memory_summaries")?;
    let marks = rows.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let content_hashes: HashMap<&str, String> = rows
        .iter()
        .map(|row| {
            (
                row.id.as_str(),
                rag_rat_query::memory::evidence::note_content_hash(&row.title, &row.body),
            )
        })
        .collect();
    let ids = rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
    let mut states = HashMap::new();

    let summary_scope = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "memory_summaries");
    let summary_sql = format!(
        "SELECT memory_id, content_hash, summary FROM memory_summaries
         WHERE memory_id IN ({marks}) AND prompt_version = ?{summary_scope}"
    );
    let mut summary_values =
        ids.iter().cloned().map(rusqlite::types::Value::Text).collect::<Vec<_>>();
    summary_values.push(rusqlite::types::Value::Text(
        rag_rat_query::memory::evidence::COMPACT_PROMPT_VERSION.to_string(),
    ));
    let mut summary_stmt = conn.prepare(&summary_sql)?;
    let summaries = summary_stmt.query_map(params_from_iter(summary_values), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    })?;
    for summary in summaries {
        let (id, content_hash, summary) = summary?;
        if content_hashes.get(id.as_str()) == Some(&content_hash) {
            states
                .entry(id)
                .or_insert_with(rag_rat_query::memory::CurrentDreamState::default)
                .summary = Some(summary);
        }
    }

    let reality_scope = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "memory_reality");
    let reality_sql = format!(
        "SELECT memory_id, content_hash, verdict, direction, evidence_json,
                checked_against_commit, checked_inputs_hash
         FROM memory_reality
         WHERE memory_id IN ({marks}) AND prompt_version = ?{reality_scope}"
    );
    let mut reality_values = ids.into_iter().map(rusqlite::types::Value::Text).collect::<Vec<_>>();
    reality_values.push(rusqlite::types::Value::Text(
        rag_rat_query::memory::evidence::VERDICT_PROMPT_VERSION.to_string(),
    ));
    let mut reality_stmt = conn.prepare(&reality_sql)?;
    let realities = reality_stmt
        .query_map(params_from_iter(reality_values), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, content_hash, verdict, direction, evidence_json, commit, stored_inputs) in realities {
        if content_hashes.get(id.as_str()) != Some(&content_hash) {
            continue;
        }
        let current_inputs =
            rag_rat_query::memory::evidence::checked_inputs_hash(conn, &id, &scope)?;
        if stored_inputs.as_deref() != Some(current_inputs.as_str()) {
            continue;
        }
        let state =
            states.entry(id).or_insert_with(rag_rat_query::memory::CurrentDreamState::default);
        state.verdict = verdict;
        state.direction = direction;
        state.evidence_json = evidence_json;
        state.checked_against_commit = commit;
    }
    Ok(states)
}

fn directory_ancestors(path: &str) -> Vec<String> {
    let mut ancestors = HashSet::from([String::new()]);
    let mut current = String::new();
    let components = Path::new(path).components().collect::<Vec<_>>();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        if let Component::Normal(value) = component {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(&value.to_string_lossy());
            ancestors.insert(current.clone());
        }
    }
    let mut ancestors = ancestors.into_iter().collect::<Vec<_>>();
    ancestors.sort();
    ancestors
}
