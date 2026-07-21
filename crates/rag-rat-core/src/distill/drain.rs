//! Queue drain for prepared distill snapshots.

use std::collections::BTreeMap;
use std::path::Path;

use rag_rat_base::locks::WriteLock;
use rag_rat_llm::chat::ChatModel;
use rag_rat_papertrail::FixEdgeSource;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::output::{self, LadderFailure, LadderResult, LadderStats, RecordOutput};
use super::prompts::{
    self, AnchorContext, FixCommit, PartnerThread, PromptBudget, PromptInput, PromptUnit,
    SymbolContext,
};
use super::{run_stats, validate};

const MAX_STORED_ERROR_CHARS: usize = 2_000;
const MAX_STORED_REPLY_CHARS: usize = 64_000;

/// Aggregate result from one bounded queue drain.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct DistillDrainReport {
    pub threads: usize,
    pub succeeded: usize,
    pub failed: usize,
    /// Model results rejected because extraction or queue identity changed during inference.
    pub stale: usize,
    pub rung_guided: u64,
    pub rung_serde: u64,
    pub rung_unguided: u64,
    pub rung_tolerant: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreadKey {
    tracker: String,
    project: String,
    item_kind: String,
    item_key: String,
}

#[derive(Debug, Clone)]
struct PreparedJob {
    queue_id: i64,
    enqueued_at_ms: i64,
    attempts: i64,
    key: ThreadKey,
    distill_input_hash: String,
    pipeline_version: i64,
    prompt_version: u32,
    fix_edge_source: FixEdgeSource,
    closing_keyword: bool,
    prompt_input: PromptInput,
    units: Vec<UnitSnapshot>,
    rendered_prompt: String,
    schema: serde_json::Value,
    model_input_hash: String,
}

struct PreparedIdentity {
    queue_id: i64,
    enqueued_at_ms: i64,
    attempts: i64,
    key: ThreadKey,
    distill_input_hash: String,
    pipeline_version: i64,
    prompt_version: u32,
    fix_edge_source: FixEdgeSource,
    closing_keyword: bool,
}

#[derive(Debug, Clone)]
struct SourceSnapshot {
    ordinal: usize,
    role: String,
    partner_ordinal: Option<usize>,
    item_kind: String,
    item_key: String,
    source_kind: String,
    source_part: String,
    source_id: String,
    exact_text: String,
    author: Option<String>,
    author_kind: Option<String>,
    author_association: Option<String>,
    created_at_ms: Option<i64>,
}

#[derive(Debug, Clone)]
struct UnitSnapshot {
    ordinal: usize,
    byte_start: usize,
    byte_end: usize,
    source: SourceSnapshot,
}

#[derive(Debug, Clone)]
struct AnchorSnapshot {
    ordinal: usize,
    kind: String,
    logical_symbol_id: Option<String>,
    file_path: Option<String>,
    name: String,
    resolved: bool,
}

pub(crate) fn drain(
    conn: &Connection,
    database_path: &Path,
    repo_id: &str,
    model: &dyn ChatModel,
    limit: usize,
    run_at_ms: i64,
) -> anyhow::Result<DistillDrainReport> {
    let budget = PromptBudget::default();
    // This read transaction ends before the first model call. Every job crossing that boundary is
    // fully owned, including the exact source text used to materialize evidence later.
    let jobs =
        in_transaction(conn, "DEFERRED", || load_prepared_jobs(conn, repo_id, limit, &budget))?;
    let mut report = DistillDrainReport { threads: jobs.len(), ..Default::default() };
    let mut aggregate_stats = LadderStats::default();

    for job in jobs {
        let outcome = output::run_output_ladder(
            model,
            &job.rendered_prompt,
            &job.schema,
            &job.prompt_input,
            &budget,
        );
        let stats = match &outcome {
            Ok(result) => result.stats,
            Err(failure) => failure.stats,
        };
        aggregate_stats.accumulate(stats);

        // The flight lock excludes another drain. The ordinary writer lock serializes only this
        // short state transition with extraction, indexing, and maintenance.
        let _write_lock = WriteLock::acquire_blocking(database_path, repo_id)?;
        let applied = match outcome {
            Ok(result) => in_transaction(conn, "IMMEDIATE", || {
                persist_success(conn, repo_id, &job, &result, &budget, run_at_ms)
            })?,
            Err(failure) => in_transaction(conn, "IMMEDIATE", || {
                persist_failure(conn, repo_id, &job, &failure, &budget)
            })?,
        };
        if !applied {
            report.stale += 1;
        } else if stats.failed == 1 {
            report.failed += 1;
        } else {
            report.succeeded += 1;
        }
    }

    report.rung_guided = aggregate_stats.rung_guided;
    report.rung_serde = aggregate_stats.rung_serde;
    report.rung_unguided = aggregate_stats.rung_unguided;
    report.rung_tolerant = aggregate_stats.rung_tolerant;
    let _write_lock = WriteLock::acquire_blocking(database_path, repo_id)?;
    in_transaction(conn, "IMMEDIATE", || {
        run_stats::record_distill_run(
            conn,
            repo_id,
            run_at_ms,
            u64::try_from(report.threads)?,
            aggregate_stats,
        )
    })?;
    Ok(report)
}

pub(crate) fn pending_count(conn: &Connection, repo_id: &str) -> anyhow::Result<u64> {
    let count = conn.query_row(
        "SELECT COUNT(*)
         FROM papertrail_distill_queue q
         JOIN papertrail_distill d
           ON d.repo_id = q.repo_id AND d.tracker = q.tracker AND d.project = q.project
          AND d.item_kind = q.item_kind AND d.item_key = q.item_key
         WHERE q.repo_id = ?1
           AND EXISTS (
               SELECT 1 FROM papertrail_distill_sources s
               WHERE s.repo_id = q.repo_id AND s.tracker = q.tracker AND s.project = q.project
                 AND s.item_kind = q.item_kind AND s.item_key = q.item_key)",
        [repo_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(u64::try_from(count)?)
}

fn load_prepared_jobs(
    conn: &Connection,
    repo_id: &str,
    limit: usize,
    budget: &PromptBudget,
) -> anyhow::Result<Vec<PreparedJob>> {
    let limit = i64::try_from(limit)?;
    let mut stmt = conn.prepare(
        "SELECT q.id, q.enqueued_at_ms, q.attempts, q.tracker, q.project, q.item_kind,
                q.item_key, d.distill_input_hash, d.pipeline_version, d.fix_edge_source,
                d.closing_keyword_floor
         FROM papertrail_distill_queue q
         JOIN papertrail_distill d
           ON d.repo_id = q.repo_id AND d.tracker = q.tracker AND d.project = q.project
          AND d.item_kind = q.item_kind AND d.item_key = q.item_key
         WHERE q.repo_id = ?1
           AND EXISTS (
               SELECT 1 FROM papertrail_distill_sources s
               WHERE s.repo_id = q.repo_id AND s.tracker = q.tracker AND s.project = q.project
                 AND s.item_kind = q.item_kind AND s.item_key = q.item_key)
         ORDER BY q.enqueued_at_ms, q.id
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![repo_id, limit], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            ThreadKey {
                tracker: row.get(3)?,
                project: row.get(4)?,
                item_kind: row.get(5)?,
                item_key: row.get(6)?,
            },
            row.get::<_, String>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, Option<String>>(10)?,
        ))
    })?;
    let mut identities = Vec::new();
    for row in rows {
        identities.push(row?);
    }

    identities
        .into_iter()
        .map(|row| {
            let (
                queue_id,
                enqueued_at_ms,
                attempts,
                key,
                input_hash,
                pipeline,
                fix_source,
                closing,
            ) = row;
            assemble_job(
                conn,
                repo_id,
                PreparedIdentity {
                    queue_id,
                    enqueued_at_ms,
                    attempts,
                    key,
                    distill_input_hash: input_hash,
                    pipeline_version: pipeline,
                    prompt_version: prompts::PROMPT_VERSION,
                    fix_edge_source: FixEdgeSource::from_db_str(&fix_source)?,
                    closing_keyword: closing.is_some(),
                },
                budget,
            )
        })
        .collect()
}

fn assemble_job(
    conn: &Connection,
    repo_id: &str,
    identity: PreparedIdentity,
    budget: &PromptBudget,
) -> anyhow::Result<PreparedJob> {
    let PreparedIdentity {
        queue_id,
        enqueued_at_ms,
        attempts,
        key,
        distill_input_hash,
        pipeline_version,
        prompt_version,
        fix_edge_source,
        closing_keyword,
    } = identity;
    let sources = load_sources(conn, repo_id, &key)?;
    let units = load_units(conn, repo_id, &key, &sources)?;
    let anchors = load_anchors(conn, repo_id, &key)?;
    let commits = load_commits(conn, repo_id, &key)?;
    let diff = load_fix_diff(conn, repo_id, &key)?;
    let xrefs = load_xrefs(conn, repo_id, &key)?;
    let prompt_input = build_prompt_input(&key, &sources, &units, &anchors, commits, xrefs, diff)?;
    let rendered_prompt = prompts::render_prompt(&prompt_input, budget);
    let schema = prompts::record_schema(&prompt_input, budget);
    let model_input_hash = compute_model_input_hash(&rendered_prompt, &schema)?;
    Ok(PreparedJob {
        queue_id,
        enqueued_at_ms,
        attempts,
        key,
        distill_input_hash,
        pipeline_version,
        prompt_version,
        fix_edge_source,
        closing_keyword,
        prompt_input,
        units,
        rendered_prompt,
        schema,
        model_input_hash,
    })
}

fn load_sources(
    conn: &Connection,
    repo_id: &str,
    key: &ThreadKey,
) -> anyhow::Result<Vec<SourceSnapshot>> {
    let mut stmt = conn.prepare(
        "SELECT source_ordinal, role, partner_ordinal, source_item_kind, source_item_key,
                source_kind, source_part, source_id, exact_text, author, author_kind,
                author_association, created_at_ms
         FROM papertrail_distill_sources
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4
           AND item_key = ?5
         ORDER BY source_ordinal",
    )?;
    let rows = stmt.query_map(
        params![repo_id, key.tracker, key.project, key.item_kind, key.item_key],
        |row| {
            Ok(SourceSnapshot {
                ordinal: usize_from_sql(row.get::<_, i64>(0)?, 0)?,
                role: row.get(1)?,
                partner_ordinal: row
                    .get::<_, Option<i64>>(2)?
                    .map(|value| usize_from_sql(value, 2))
                    .transpose()?,
                item_kind: row.get(3)?,
                item_key: row.get(4)?,
                source_kind: row.get(5)?,
                source_part: row.get(6)?,
                source_id: row.get(7)?,
                exact_text: row.get(8)?,
                author: row.get(9)?,
                author_kind: row.get(10)?,
                author_association: row.get(11)?,
                created_at_ms: row.get(12)?,
            })
        },
    )?;
    let mut sources = Vec::new();
    for row in rows {
        sources.push(row?);
    }
    for (expected, source) in sources.iter().enumerate() {
        anyhow::ensure!(source.ordinal == expected, "distill source ordinals are not contiguous");
    }
    Ok(sources)
}

fn load_units(
    conn: &Connection,
    repo_id: &str,
    key: &ThreadKey,
    sources: &[SourceSnapshot],
) -> anyhow::Result<Vec<UnitSnapshot>> {
    let by_ordinal: BTreeMap<usize, &SourceSnapshot> =
        sources.iter().map(|source| (source.ordinal, source)).collect();
    let mut stmt = conn.prepare(
        "SELECT unit_ordinal, source_ordinal, byte_start, byte_end
         FROM papertrail_distill_units
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4
           AND item_key = ?5
         ORDER BY unit_ordinal",
    )?;
    let rows = stmt.query_map(
        params![repo_id, key.tracker, key.project, key.item_kind, key.item_key],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    let mut units = Vec::new();
    for row in rows {
        let (ordinal, source_ordinal, byte_start, byte_end) = row?;
        let ordinal = usize::try_from(ordinal)?;
        let source_ordinal = usize::try_from(source_ordinal)?;
        let byte_start = usize::try_from(byte_start)?;
        let byte_end = usize::try_from(byte_end)?;
        let source = (*by_ordinal
            .get(&source_ordinal)
            .ok_or_else(|| anyhow::anyhow!("distill unit references missing source ordinal"))?)
        .clone();
        anyhow::ensure!(
            byte_start < byte_end
                && source.exact_text.is_char_boundary(byte_start)
                && source.exact_text.is_char_boundary(byte_end)
                && byte_end <= source.exact_text.len(),
            "distill unit has an invalid source byte span"
        );
        anyhow::ensure!(ordinal == units.len(), "distill unit ordinals are not contiguous");
        units.push(UnitSnapshot { ordinal, byte_start, byte_end, source });
    }
    Ok(units)
}

fn load_anchors(
    conn: &Connection,
    repo_id: &str,
    key: &ThreadKey,
) -> anyhow::Result<Vec<AnchorSnapshot>> {
    let mut stmt = conn.prepare(
        "SELECT candidate_ordinal, anchor_kind, logical_symbol_id, file_path, name, resolved
         FROM papertrail_distill_anchors
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4
           AND item_key = ?5
         ORDER BY candidate_ordinal",
    )?;
    let rows = stmt.query_map(
        params![repo_id, key.tracker, key.project, key.item_kind, key.item_key],
        |row| {
            Ok(AnchorSnapshot {
                ordinal: usize_from_sql(row.get::<_, i64>(0)?, 0)?,
                kind: row.get(1)?,
                logical_symbol_id: row.get(2)?,
                file_path: row.get(3)?,
                name: row.get(4)?,
                resolved: row.get::<_, i64>(5)? != 0,
            })
        },
    )?;
    let mut anchors = Vec::new();
    for row in rows {
        anchors.push(row?);
    }
    Ok(anchors)
}

fn load_commits(
    conn: &Connection,
    repo_id: &str,
    key: &ThreadKey,
) -> anyhow::Result<Vec<FixCommit>> {
    let mut stmt = conn.prepare(
        "SELECT rc.commit_sha, gc.subject, gc.body
         FROM papertrail_distill_record_commits rc
         LEFT JOIN git_commits gc ON gc.repo_id = rc.repo_id AND gc.hash = rc.commit_sha
         WHERE rc.repo_id = ?1 AND rc.tracker = ?2 AND rc.project = ?3 AND rc.item_kind = ?4
           AND rc.item_key = ?5
         ORDER BY rc.commit_sha",
    )?;
    let rows = stmt.query_map(
        params![repo_id, key.tracker, key.project, key.item_kind, key.item_key],
        |row| {
            let sha: String = row.get(0)?;
            let subject: Option<String> = row.get(1)?;
            let body: Option<String> = row.get(2)?;
            let message = match (subject, body) {
                (Some(subject), Some(body)) if !body.is_empty() => format!("{subject}\n\n{body}"),
                (Some(subject), _) => subject,
                _ => String::new(),
            };
            Ok(FixCommit { sha, message })
        },
    )?;
    let mut commits = Vec::new();
    for row in rows {
        commits.push(row?);
    }
    Ok(commits)
}

/// The snapshotted per-file patches (#800), concatenated in deterministic (commit, path) order —
/// the extraction froze the diff, so the drain renders it without ever opening the repo.
fn load_fix_diff(
    conn: &Connection,
    repo_id: &str,
    key: &ThreadKey,
) -> anyhow::Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT patch FROM papertrail_distill_fix_diffs
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4
           AND item_key = ?5
         ORDER BY commit_sha, path",
    )?;
    let rows = stmt.query_map(
        params![repo_id, key.tracker, key.project, key.item_kind, key.item_key],
        |row| row.get::<_, String>(0),
    )?;
    let mut diff = String::new();
    for row in rows {
        diff.push_str(&row?);
        if !diff.ends_with('\n') {
            diff.push('\n');
        }
    }
    Ok((!diff.is_empty()).then_some(diff))
}

/// The snapshotted cross-referenced items (#800), in extraction's durable ordinal order.
fn load_xrefs(
    conn: &Connection,
    repo_id: &str,
    key: &ThreadKey,
) -> anyhow::Result<Vec<prompts::Xref>> {
    let mut stmt = conn.prepare(
        "SELECT target_item_kind, target_item_key, ref_kind, title, opening
         FROM papertrail_distill_xrefs
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4
           AND item_key = ?5
         ORDER BY xref_ordinal",
    )?;
    let rows = stmt.query_map(
        params![repo_id, key.tracker, key.project, key.item_kind, key.item_key],
        |row| {
            Ok(prompts::Xref {
                kind: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                key: row.get(1)?,
                ref_kind: row.get(2)?,
                title: row.get(3)?,
                opening: row.get(4)?,
            })
        },
    )?;
    let mut xrefs = Vec::new();
    for row in rows {
        xrefs.push(row?);
    }
    Ok(xrefs)
}

fn build_prompt_input(
    key: &ThreadKey,
    sources: &[SourceSnapshot],
    units: &[UnitSnapshot],
    anchors: &[AnchorSnapshot],
    commits: Vec<FixCommit>,
    xrefs: Vec<prompts::Xref>,
    diff: Option<String>,
) -> anyhow::Result<PromptInput> {
    let primary_title = sources
        .iter()
        .find(|source| source.role == "primary" && source.source_part == "title")
        .ok_or_else(|| anyhow::anyhow!("prepared distill snapshot has no primary title"))?;
    let primary_units: Vec<PromptUnit> =
        units.iter().filter(|unit| unit.source.role == "primary").map(prompt_unit).collect();
    for (expected, unit) in units.iter().filter(|unit| unit.source.role == "primary").enumerate() {
        anyhow::ensure!(unit.ordinal == expected, "primary distill unit ids are not a prefix");
    }

    // Every coalesced partner, in the extraction's durable partner_ordinal order — the persisted
    // ordinal, not row-id or query order, defines the render sequence.
    let partner_ordinals: std::collections::BTreeSet<usize> =
        sources.iter().filter_map(|source| source.partner_ordinal).collect();
    let partners = partner_ordinals
        .into_iter()
        .map(|partner_ordinal| {
            let partner_sources: Vec<&SourceSnapshot> = sources
                .iter()
                .filter(|source| source.partner_ordinal == Some(partner_ordinal))
                .collect();
            let title = partner_sources
                .iter()
                .find(|source| source.source_part == "title")
                .map_or("", |source| source.exact_text.as_str());
            let identity = partner_sources.first().copied();
            PartnerThread {
                kind: identity
                    .map_or("change_request", |source| source.item_kind.as_str())
                    .to_string(),
                key: identity.map_or("", |source| source.item_key.as_str()).to_string(),
                title: title.to_string(),
                units: units
                    .iter()
                    .filter(|unit| unit.source.partner_ordinal == Some(partner_ordinal))
                    .map(prompt_unit)
                    .collect(),
            }
        })
        .collect();
    let anchor_candidates = anchors
        .iter()
        .map(|anchor| AnchorContext {
            index: anchor.ordinal,
            kind: anchor.kind.clone(),
            name: anchor.name.clone(),
            file: anchor.file_path.clone(),
            logical_symbol_id: anchor.logical_symbol_id.clone(),
        })
        .collect();
    let symbols = anchors
        .iter()
        .filter(|anchor| anchor.kind == "symbol" && anchor.resolved)
        .map(|anchor| SymbolContext {
            name: anchor.name.clone(),
            kind: anchor.kind.clone(),
            file: anchor.file_path.clone().unwrap_or_default(),
        })
        .collect();
    Ok(PromptInput {
        kind: key.item_kind.clone(),
        key: key.item_key.clone(),
        merged: key.item_kind == "change_request",
        title: primary_title.exact_text.clone(),
        opened: primary_title.created_at_ms.map_or_else(String::new, |value| value.to_string()),
        units: primary_units,
        partners,
        xrefs,
        fix_commits: commits,
        symbols,
        anchor_candidates,
        diff,
    })
}

fn prompt_unit(unit: &UnitSnapshot) -> PromptUnit {
    PromptUnit {
        text: unit.source.exact_text[unit.byte_start..unit.byte_end].to_string(),
        source: if unit.source.source_kind == "comment" {
            format!("comment {}", unit.source.source_id)
        } else {
            format!(
                "{} #{} {}",
                unit.source.item_kind, unit.source.item_key, unit.source.source_part
            )
        },
    }
}

fn compute_model_input_hash(
    rendered_prompt: &str,
    schema: &serde_json::Value,
) -> anyhow::Result<String> {
    let schema = serde_json::to_vec(schema)?;
    let mut hasher = Sha256::new();
    hash_bytes(&mut hasher, b"rag-rat-distill-model-input-v1");
    hash_bytes(&mut hasher, rendered_prompt.as_bytes());
    hash_bytes(&mut hasher, &schema);
    let hex: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("sha256:{hex}"))
}

fn hash_bytes(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn current_job_matches(
    conn: &Connection,
    repo_id: &str,
    expected: &PreparedJob,
    budget: &PromptBudget,
) -> anyhow::Result<bool> {
    let current = load_job_by_queue_id(conn, repo_id, expected.queue_id, budget)?;
    Ok(current.is_some_and(|current| {
        current.enqueued_at_ms == expected.enqueued_at_ms
            && current.attempts == expected.attempts
            && current.key == expected.key
            && current.distill_input_hash == expected.distill_input_hash
            && current.pipeline_version == expected.pipeline_version
            && current.prompt_version == expected.prompt_version
            && current.model_input_hash == expected.model_input_hash
    }))
}

fn load_job_by_queue_id(
    conn: &Connection,
    repo_id: &str,
    queue_id: i64,
    budget: &PromptBudget,
) -> anyhow::Result<Option<PreparedJob>> {
    let row = conn
        .query_row(
            "SELECT q.enqueued_at_ms, q.attempts, q.tracker, q.project, q.item_kind, q.item_key,
                    d.distill_input_hash, d.pipeline_version, d.fix_edge_source,
                    d.closing_keyword_floor
             FROM papertrail_distill_queue q
             JOIN papertrail_distill d
               ON d.repo_id = q.repo_id AND d.tracker = q.tracker AND d.project = q.project
              AND d.item_kind = q.item_kind AND d.item_key = q.item_key
             WHERE q.repo_id = ?1 AND q.id = ?2",
            params![repo_id, queue_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    ThreadKey {
                        tracker: row.get(2)?,
                        project: row.get(3)?,
                        item_kind: row.get(4)?,
                        item_key: row.get(5)?,
                    },
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            },
        )
        .optional()?;
    row.map(|(enqueued, attempts, key, hash, pipeline, fix_source, closing)| {
        assemble_job(
            conn,
            repo_id,
            PreparedIdentity {
                queue_id,
                enqueued_at_ms: enqueued,
                attempts,
                key,
                distill_input_hash: hash,
                pipeline_version: pipeline,
                prompt_version: prompts::PROMPT_VERSION,
                fix_edge_source: FixEdgeSource::from_db_str(&fix_source)?,
                closing_keyword: closing.is_some(),
            },
            budget,
        )
    })
    .transpose()
}

fn persist_success(
    conn: &Connection,
    repo_id: &str,
    job: &PreparedJob,
    result: &LadderResult,
    budget: &PromptBudget,
    distilled_at_ms: i64,
) -> anyhow::Result<bool> {
    if !current_job_matches(conn, repo_id, job, budget)? {
        return Ok(false);
    }
    let output = &result.output;
    let evidence = collect_evidence(job, output)?;
    let decision_associations: Vec<Option<String>> = evidence
        .iter()
        .filter(|evidence| evidence.field == "decision")
        .map(|evidence| evidence.source.author_association.clone())
        .collect();
    let outcome_verified = validate::outcome_claim_verified(
        output.outcome.status,
        job.fix_edge_source != FixEdgeSource::None,
        job.closing_keyword,
    );
    let updated = conn.execute(
        "UPDATE papertrail_distill SET
             root_issue = ?1, root_cause = ?2, root_cause_class = ?3, decision_chosen = ?4,
             outcome_summary = ?5, outcome_status_model = ?6,
             epistemic_status_decision = NULL, epistemic_status_outcome = NULL,
             quotes_materialized = ?7, outcome_claim_verified = ?8,
             decision_provenance_verified = ?9, prompt_version = ?10, model_input_hash = ?11,
             distilled_at_ms = ?12
         WHERE repo_id = ?13 AND tracker = ?14 AND project = ?15 AND item_kind = ?16
           AND item_key = ?17 AND distill_input_hash = ?18 AND pipeline_version = ?19",
        params![
            output.root_issue,
            output.root_cause,
            output.root_cause_class,
            output.decision.chosen,
            output.outcome.summary,
            output.outcome.status.as_db_str(),
            i64::try_from(evidence.len())?,
            outcome_verified,
            validate::decision_provenance_verified(&decision_associations),
            i64::from(job.prompt_version),
            job.model_input_hash,
            distilled_at_ms,
            repo_id,
            job.key.tracker,
            job.key.project,
            job.key.item_kind,
            job.key.item_key,
            job.distill_input_hash,
            job.pipeline_version,
        ],
    )?;
    if updated != 1 {
        return Ok(false);
    }
    clear_model_junctions(conn, repo_id, &job.key)?;
    for evidence in evidence {
        insert_evidence(conn, repo_id, &job.key, &evidence)?;
    }
    for (ordinal, rejected) in output.decision.rejected.iter().enumerate() {
        conn.execute(
            "INSERT INTO papertrail_distill_alternatives
                 (tracker, project, item_kind, item_key, ordinal, alternative, reason, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                job.key.tracker,
                job.key.project,
                job.key.item_kind,
                job.key.item_key,
                i64::try_from(ordinal)?,
                rejected.alternative,
                rejected.reason,
                repo_id,
            ],
        )?;
    }
    conn.execute(
        "UPDATE papertrail_distill_anchors SET selected = 0
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5",
        params![repo_id, job.key.tracker, job.key.project, job.key.item_kind, job.key.item_key],
    )?;
    for ordinal in &output.anchor_indices {
        let selected = conn.execute(
            "UPDATE papertrail_distill_anchors SET selected = 1
             WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4
               AND item_key = ?5 AND candidate_ordinal = ?6",
            params![
                repo_id,
                job.key.tracker,
                job.key.project,
                job.key.item_kind,
                job.key.item_key,
                i64::try_from(*ordinal)?,
            ],
        )?;
        anyhow::ensure!(selected == 1, "validated anchor candidate vanished during persistence");
    }
    let deleted = conn.execute(
        "DELETE FROM papertrail_distill_queue
         WHERE repo_id = ?1 AND id = ?2 AND enqueued_at_ms = ?3 AND attempts = ?4",
        params![repo_id, job.queue_id, job.enqueued_at_ms, job.attempts],
    )?;
    anyhow::ensure!(deleted == 1, "distill queue identity changed inside success transaction");
    Ok(true)
}

fn persist_failure(
    conn: &Connection,
    repo_id: &str,
    job: &PreparedJob,
    failure: &LadderFailure,
    budget: &PromptBudget,
) -> anyhow::Result<bool> {
    if !current_job_matches(conn, repo_id, job, budget)? {
        return Ok(false);
    }
    let error = bound_chars(&failure.errors.join("; "), MAX_STORED_ERROR_CHARS);
    let raw_reply =
        failure.final_raw_reply.as_deref().map(|reply| bound_chars(reply, MAX_STORED_REPLY_CHARS));
    let updated = conn.execute(
        "UPDATE papertrail_distill_queue
         SET attempts = attempts + 1, last_error = ?1, raw_reply = ?2
         WHERE repo_id = ?3 AND id = ?4 AND enqueued_at_ms = ?5 AND attempts = ?6",
        params![error, raw_reply, repo_id, job.queue_id, job.enqueued_at_ms, job.attempts],
    )?;
    Ok(updated == 1)
}

struct EvidenceRow<'a> {
    field: &'static str,
    unit: &'a UnitSnapshot,
    source: &'a SourceSnapshot,
}

fn collect_evidence<'a>(
    job: &'a PreparedJob,
    output: &RecordOutput,
) -> anyhow::Result<Vec<EvidenceRow<'a>>> {
    let mut evidence = Vec::new();
    for (field, citations) in [
        ("root_cause", &output.root_cause_units),
        ("decision", &output.decision_units),
        ("outcome", &output.outcome_units),
    ] {
        for citation in citations {
            let unit = job
                .units
                .get(citation.get())
                .ok_or_else(|| anyhow::anyhow!("validated citation has no snapshot unit"))?;
            anyhow::ensure!(unit.source.role == "primary", "partner unit cannot be evidence");
            evidence.push(EvidenceRow { field, unit, source: &unit.source });
        }
    }
    Ok(evidence)
}

fn insert_evidence(
    conn: &Connection,
    repo_id: &str,
    key: &ThreadKey,
    evidence: &EvidenceRow<'_>,
) -> anyhow::Result<()> {
    let quote = &evidence.source.exact_text[evidence.unit.byte_start..evidence.unit.byte_end];
    conn.execute(
        "INSERT INTO papertrail_distill_evidence
             (tracker, project, item_kind, item_key, field, source_kind, source_id, byte_start,
              byte_end, quote, author, author_kind, author_association, unit_created_at_ms, \
         repo_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            key.tracker,
            key.project,
            key.item_kind,
            key.item_key,
            evidence.field,
            evidence.source.source_kind,
            evidence.source.source_id,
            i64::try_from(evidence.unit.byte_start)?,
            i64::try_from(evidence.unit.byte_end)?,
            quote,
            evidence.source.author,
            evidence.source.author_kind,
            evidence.source.author_association,
            evidence.source.created_at_ms,
            repo_id,
        ],
    )?;
    Ok(())
}

fn clear_model_junctions(conn: &Connection, repo_id: &str, key: &ThreadKey) -> anyhow::Result<()> {
    for table in ["papertrail_distill_evidence", "papertrail_distill_alternatives"] {
        conn.execute(
            &format!(
                "DELETE FROM {table} WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3
                 AND item_kind = ?4 AND item_key = ?5"
            ),
            params![repo_id, key.tracker, key.project, key.item_kind, key.item_key],
        )?;
    }
    Ok(())
}

fn usize_from_sql(value: i64, column: usize) -> rusqlite::Result<usize> {
    usize::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn bound_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn in_transaction<T>(
    conn: &Connection,
    mode: &str,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    conn.execute_batch(&format!("BEGIN {mode}"))?;
    match f() {
        Ok(value) =>
            if let Err(error) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(value)
            },
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use rag_rat_db::schema::{
        apply_distill_anchor_selection, apply_distill_enriched_context, apply_distill_record_store,
        apply_distill_safe_input_snapshot,
    };
    use rag_rat_llm::chat::{ChatModel, GuidedJson};
    use rusqlite::Connection;

    use super::{PromptBudget, compute_model_input_hash, drain, load_prepared_jobs, pending_count};
    use crate::distill::prompts;

    struct ScriptedModel {
        replies: Mutex<VecDeque<anyhow::Result<String>>>,
        before_first: Option<Box<dyn Fn() + Send + Sync>>,
        calls: Mutex<usize>,
    }

    impl ScriptedModel {
        fn new(replies: Vec<anyhow::Result<String>>) -> Self {
            Self { replies: Mutex::new(replies.into()), before_first: None, calls: Mutex::new(0) }
        }

        fn before_first(mut self, action: impl Fn() + Send + Sync + 'static) -> Self {
            self.before_first = Some(Box::new(action));
            self
        }
    }

    impl ChatModel for ScriptedModel {
        fn complete_guided(
            &self,
            _prompt: &str,
            _guided: Option<GuidedJson<'_>>,
        ) -> anyhow::Result<String> {
            let mut calls = self.calls.lock().unwrap();
            if *calls == 0
                && let Some(action) = &self.before_first
            {
                action();
            }
            *calls += 1;
            self.replies.lock().unwrap().pop_front().unwrap()
        }

        fn model_id(&self) -> &str {
            "scripted"
        }
    }

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_distill_record_store(&conn).unwrap();
        apply_distill_anchor_selection(&conn).unwrap();
        apply_distill_safe_input_snapshot(&conn).unwrap();
        apply_distill_enriched_context(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE git_commits(
                 hash TEXT NOT NULL, subject TEXT NOT NULL, body TEXT NOT NULL,
                 repo_id TEXT NOT NULL, PRIMARY KEY(repo_id, hash)
             ) STRICT;",
        )
        .unwrap();
        conn
    }

    fn seed(conn: &Connection, key: &str, enqueued_at_ms: i64) {
        conn.execute(
            "INSERT INTO papertrail_distill
                 (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
                  fix_edge_source, thread_shape, distilled_at_ms, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, ?2, 2, 'provider', 'investigation', 1,
                     'repo')",
            rusqlite::params![key, format!("sha256:input-{key}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_queue
                 (tracker, project, item_kind, item_key, enqueued_at_ms, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, ?2, 'repo')",
            rusqlite::params![key, enqueued_at_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_sources
                 (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                  source_item_kind, source_item_key, source_kind, source_part, source_id,
                  exact_text, author, author_association, created_at_ms, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, 0, 'primary', NULL, 'issue', ?1, 'item',
                     'title', ?1, 'A title', 'owner', 'OWNER', 10, 'repo'),
                    ('github', 'org/repo', 'issue', ?1, 1, 'primary', NULL, 'issue', ?1, 'item',
                     'body', ?1, 'Cause and decision landed.', 'owner', 'OWNER', 10, 'repo')",
            [key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_units
                 (tracker, project, item_kind, item_key, unit_ordinal, source_ordinal, byte_start,
                  byte_end, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, 0, 1, 0, 26, 'repo')",
            [key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, file_path,
                  name, resolved, candidate_ordinal, selected, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, 'symbol', 'sym_1', 'src/lib.rs', 'run', 1,
                     0, 0, 'repo')",
            [key],
        )
        .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO git_commits(hash, subject, body, repo_id)
             VALUES ('abc', 'Fix it', 'Detailed fix.', 'repo')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_record_commits
                 (tracker, project, item_kind, item_key, commit_sha, repo_id)
             VALUES ('github', 'org/repo', 'issue', ?1, 'abc', 'repo')",
            [key],
        )
        .unwrap();
    }

    fn valid_reply() -> String {
        serde_json::json!({
            "root_issue": "The operation failed.",
            "root_cause_units": [0],
            "root_cause": "A stale decision caused the failure.",
            "root_cause_class": "stale decision",
            "decision_units": [0],
            "decision": {
                "chosen": "Use the current decision.",
                "rejected": [{"alternative": "Keep stale state", "reason": "It fails."}]
            },
            "outcome_units": [0],
            "anchor_indices": [0],
            "outcome": {"status": "landed", "summary": "The fix landed."}
        })
        .to_string()
    }

    #[test]
    fn successful_drain_persists_the_complete_model_transition() {
        let conn = fixture();
        let lock_db = tempfile::NamedTempFile::new().unwrap();
        seed(&conn, "2", 20);
        let report = drain(
            &conn,
            lock_db.path(),
            "repo",
            &ScriptedModel::new(vec![Ok(valid_reply())]),
            10,
            99,
        )
        .unwrap();
        assert_eq!((report.threads, report.succeeded, report.failed, report.stale), (1, 1, 0, 0));
        let row: (String, String, i64, i64, i64, i64, String) = conn
            .query_row(
                "SELECT root_cause, outcome_status_model, quotes_materialized,
                        decision_provenance_verified, outcome_claim_verified, prompt_version,
                        model_input_hash
                 FROM papertrail_distill WHERE repo_id = 'repo' AND item_key = '2'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, "A stale decision caused the failure.");
        assert_eq!(row.1, "landed");
        assert_eq!((row.2, row.3, row.4, row.5), (3, 1, 1, i64::from(prompts::PROMPT_VERSION)));
        assert!(row.6.starts_with("sha256:"));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM papertrail_distill_queue", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM papertrail_distill_evidence", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            conn.query_row("SELECT quote FROM papertrail_distill_evidence LIMIT 1", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "Cause and decision landed."
        );
        assert_eq!(
            conn.query_row("SELECT selected FROM papertrail_distill_anchors", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT threads, rung_guided, rung_serde, failed
                 FROM papertrail_distill_runs WHERE repo_id = 'repo'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap(),
            (1, 1, 1, 0)
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM papertrail_distill_record_commits", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            1,
            "the drain never rewrites mechanical fixing commits"
        );
    }

    #[test]
    fn pending_count_includes_only_prepared_work_for_the_requested_repo() {
        let conn = fixture();
        assert_eq!(pending_count(&conn, "repo").unwrap(), 0);
        seed(&conn, "2", 20);
        assert_eq!(pending_count(&conn, "repo").unwrap(), 1);
        assert_eq!(pending_count(&conn, "other").unwrap(), 0);

        conn.execute("DELETE FROM papertrail_distill_sources WHERE repo_id = 'repo'", []).unwrap();
        assert_eq!(pending_count(&conn, "repo").unwrap(), 0);
    }

    #[test]
    fn failed_ladder_increments_attempt_and_bounds_diagnostics() {
        let conn = fixture();
        let lock_db = tempfile::NamedTempFile::new().unwrap();
        seed(&conn, "2", 20);
        let huge = "x".repeat(70_000);
        let report = drain(
            &conn,
            lock_db.path(),
            "repo",
            &ScriptedModel::new(vec![Ok("not json".into()), Ok(huge)]),
            10,
            99,
        )
        .unwrap();
        assert_eq!((report.failed, report.stale), (1, 0));
        let (attempts, error, raw): (i64, String, String) = conn
            .query_row(
                "SELECT attempts, last_error, raw_reply FROM papertrail_distill_queue",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(attempts, 1);
        assert!(error.chars().count() <= 2_000);
        assert_eq!(raw.chars().count(), 64_000);
    }

    #[test]
    fn stale_success_does_not_delete_or_poison_new_work() {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        let conn = Connection::open(&path).unwrap();
        apply_distill_record_store(&conn).unwrap();
        apply_distill_anchor_selection(&conn).unwrap();
        apply_distill_safe_input_snapshot(&conn).unwrap();
        apply_distill_enriched_context(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE git_commits(
                 hash TEXT NOT NULL, subject TEXT NOT NULL, body TEXT NOT NULL,
                 repo_id TEXT NOT NULL, PRIMARY KEY(repo_id, hash)
             ) STRICT; PRAGMA journal_mode = WAL;",
        )
        .unwrap();
        seed(&conn, "2", 20);
        let path_for_model = path.to_path_buf();
        let model = ScriptedModel::new(vec![Ok(valid_reply())]).before_first(move || {
            let other = Connection::open(&path_for_model).unwrap();
            other
                .execute(
                    "UPDATE papertrail_distill SET distill_input_hash = 'sha256:new-input'
                     WHERE repo_id = 'repo' AND item_key = '2'",
                    [],
                )
                .unwrap();
        });
        let report = drain(&conn, path.as_ref(), "repo", &model, 10, 99).unwrap();
        assert_eq!((report.succeeded, report.stale), (0, 1));
        let (root_cause, attempts): (Option<String>, i64) = conn
            .query_row(
                "SELECT d.root_cause, q.attempts FROM papertrail_distill d
                 JOIN papertrail_distill_queue q USING(repo_id, tracker, project, item_kind, \
                 item_key)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((root_cause, attempts), (None, 0));
    }

    #[test]
    fn loading_is_ordered_limited_and_hashes_exact_visible_input() {
        let conn = fixture();
        seed(&conn, "later", 20);
        seed(&conn, "first", 10);
        // Insert partner rows out of row-id order. The persisted partner ordinal, not insertion
        // order, defines the render sequence.
        conn.execute_batch(
            "INSERT INTO papertrail_distill_sources
                 (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                  source_item_kind, source_item_key, source_kind, source_part, source_id,
                  exact_text, repo_id)
             VALUES ('github', 'org/repo', 'issue', 'first', 4, 'partner', 1,
                     'change_request', 'later-partner', 'item', 'title', 'later-partner',
                     'Later partner', 'repo'),
                    ('github', 'org/repo', 'issue', 'first', 5, 'partner', 1,
                     'change_request', 'later-partner', 'item', 'body', 'later-partner',
                     'Later partner body', 'repo'),
                    ('github', 'org/repo', 'issue', 'first', 2, 'partner', 0,
                     'change_request', 'first-partner', 'item', 'title', 'first-partner',
                     'First partner', 'repo'),
                    ('github', 'org/repo', 'issue', 'first', 3, 'partner', 0,
                     'change_request', 'first-partner', 'item', 'body', 'first-partner',
                     'First partner body', 'repo');",
        )
        .unwrap();
        let budget = PromptBudget::default();
        let jobs = load_prepared_jobs(&conn, "repo", 1, &budget).unwrap();
        assert_eq!(jobs[0].key.item_key, "first");
        assert!(jobs[0].rendered_prompt.contains("Detailed fix."));
        assert!(jobs[0].rendered_prompt.contains("[A0]"));
        // Every coalesced partner renders, lowest partner_ordinal first (#800).
        let first_pos = jobs[0].rendered_prompt.find("First partner").expect("first partner");
        let later_pos = jobs[0].rendered_prompt.find("Later partner").expect("later partner");
        assert!(first_pos < later_pos, "partners render in durable ordinal order");
        assert_eq!(
            jobs[0].model_input_hash,
            compute_model_input_hash(&jobs[0].rendered_prompt, &jobs[0].schema).unwrap()
        );
        let again = load_prepared_jobs(&conn, "repo", 1, &budget).unwrap();
        assert_eq!(jobs[0].model_input_hash, again[0].model_input_hash);
        conn.execute(
            "UPDATE git_commits SET body = 'Changed model-visible body' WHERE repo_id = 'repo'",
            [],
        )
        .unwrap();
        let changed = load_prepared_jobs(&conn, "repo", 1, &budget).unwrap();
        assert_ne!(jobs[0].model_input_hash, changed[0].model_input_hash);
    }

    #[test]
    fn enriched_context_renders_from_the_frozen_snapshots() {
        let conn = fixture();
        seed(&conn, "first", 10);
        conn.execute(
            "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES ('github', 'org/repo', 'issue', 'first', 'abc123', 'src/widget.rs',
                     'diff --git a/src/widget.rs b/src/widget.rs\n--- a/src/widget.rs\n+++ \
             b/src/widget.rs\n@@ -1 +1 @@\n-old\n+new\n',
                     'repo')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_xrefs
                 (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                  target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                  repo_id)
             VALUES ('github', 'org/repo', 'issue', 'first', 0, 'github', 'org/repo', 'issue',
                     '9', 'reference', 'Related refactor', 'We reworked the render path.', 'repo')",
            [],
        )
        .unwrap();

        let jobs = load_prepared_jobs(&conn, "repo", 1, &PromptBudget::default()).unwrap();
        let prompt = &jobs[0].rendered_prompt;
        assert!(prompt.contains("DIFF:"), "the diff block renders: {prompt}");
        assert!(prompt.contains("+new"), "hunk content renders: {prompt}");
        assert!(prompt.contains("REFERENCED ITEMS:"), "{prompt}");
        assert!(
            prompt.contains(
                "[issue] #9 (reference): Related refactor — We reworked the render path."
            ),
            "{prompt}"
        );
    }

    #[test]
    fn fix_diff_rows_concatenate_in_commit_path_order_with_newline_separators() {
        // `load_fix_diff` re-sorts the persisted per-file patches by (commit_sha, path) and joins
        // them, inserting a separating newline after any row that does not already end in one.
        let conn = fixture();
        seed(&conn, "first", 10);
        // Inserted out of (commit_sha, path) order; the SECOND row (commit 'bbb') has no trailing
        // newline, exercising the normalization branch.
        conn.execute(
            "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES ('github','org/repo','issue','first','bbb','src/z.rs','PATCH-Z-no-newline',
                     'repo')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES ('github','org/repo','issue','first','aaa','src/a.rs','PATCH-A' || char(10),
                     'repo')",
            [],
        )
        .unwrap();

        let jobs = load_prepared_jobs(&conn, "repo", 1, &PromptBudget::default()).unwrap();
        let prompt = &jobs[0].rendered_prompt;
        let a_pos = prompt.find("PATCH-A").expect("patch A renders");
        let z_pos = prompt.find("PATCH-Z-no-newline").expect("patch Z renders");
        assert!(a_pos < z_pos, "rows render in (commit_sha, path) order: aaa before bbb: {prompt}");
    }

    #[test]
    fn a_null_xref_target_kind_renders_an_empty_kind_label() {
        // `load_xrefs` tolerates a NULL `target_item_kind` (defaulting it to an empty label) rather
        // than dropping the row — the drain never fabricates a kind the snapshot did not resolve.
        let conn = fixture();
        seed(&conn, "first", 10);
        conn.execute(
            "INSERT INTO papertrail_distill_xrefs
                 (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                  target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                  repo_id)
             VALUES \
             ('github','org/repo','issue','first',0,'github','org/repo',NULL,'9','reference',
                     'Kindless target','','repo')",
            [],
        )
        .unwrap();

        let jobs = load_prepared_jobs(&conn, "repo", 1, &PromptBudget::default()).unwrap();
        let prompt = &jobs[0].rendered_prompt;
        assert!(prompt.contains("REFERENCED ITEMS:"), "{prompt}");
        assert!(
            prompt.contains("[] #9 (reference): Kindless target"),
            "a NULL target kind renders as an empty kind label: {prompt}",
        );
    }

    #[test]
    fn model_executes_after_the_read_transaction_is_released() {
        let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        let conn = Connection::open(&path).unwrap();
        apply_distill_record_store(&conn).unwrap();
        apply_distill_anchor_selection(&conn).unwrap();
        apply_distill_safe_input_snapshot(&conn).unwrap();
        apply_distill_enriched_context(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE git_commits(
                 hash TEXT NOT NULL, subject TEXT NOT NULL, body TEXT NOT NULL,
                 repo_id TEXT NOT NULL, PRIMARY KEY(repo_id, hash)
             ) STRICT;
             CREATE TABLE model_probe(value INTEGER NOT NULL);
             PRAGMA journal_mode = WAL;",
        )
        .unwrap();
        seed(&conn, "2", 20);
        let path_for_model = path.to_path_buf();
        let model = ScriptedModel::new(vec![Ok(valid_reply())]).before_first(move || {
            let other = Connection::open(&path_for_model).unwrap();
            other.execute("INSERT INTO model_probe VALUES (1)", []).unwrap();
        });
        let report = drain(&conn, path.as_ref(), "repo", &model, 10, 99).unwrap();
        assert_eq!(report.succeeded, 1);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM model_probe", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
