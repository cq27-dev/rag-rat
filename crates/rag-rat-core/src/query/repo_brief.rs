use std::collections::BTreeMap;
use std::fmt;

use rusqlite::{Connection, OptionalExtension, Row};
use serde::Serialize;

use crate::index::table_row_count;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoBriefMode {
    Spine,
    Churn,
    GodModules,
    RefactorCandidates,
}

impl RepoBriefMode {
    pub fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        match value.unwrap_or("spine") {
            "spine" => Ok(Self::Spine),
            "churn" => Ok(Self::Churn),
            "god_modules" => Ok(Self::GodModules),
            "refactor_candidates" => Ok(Self::RefactorCandidates),
            other => anyhow::bail!(
                "unknown repo brief mode `{other}`; expected spine, churn, god_modules, or \
                 refactor_candidates"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spine => "spine",
            Self::Churn => "churn",
            Self::GodModules => "god_modules",
            Self::RefactorCandidates => "refactor_candidates",
        }
    }
}

impl fmt::Display for RepoBriefMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RepoBriefOptions {
    pub mode: RepoBriefMode,
    pub limit: u32,
    pub include_generated: bool,
    pub include_memories: bool,
}

impl Default for RepoBriefOptions {
    fn default() -> Self {
        Self {
            mode: RepoBriefMode::Spine,
            limit: 10,
            include_generated: false,
            include_memories: true,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RepoBrief {
    pub mode: String,
    pub summary: RepoBriefSummary,
    pub candidates: Vec<RepoBriefCandidate>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RepoBriefSummary {
    pub total_files: u64,
    pub returned_files: u64,
    pub generated_files: u64,
    pub graph_edges: u64,
    pub git_commits: u64,
    pub git_file_changes: u64,
    pub repo_memories: RepoBriefMemoryCounts,
    pub scoring_note: String,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct RepoBriefMemoryCounts {
    pub active: u64,
    pub stale: u64,
    pub obsolete: u64,
    pub other: u64,
}

#[derive(Debug, Serialize)]
pub struct RepoBriefCandidate {
    pub path: String,
    #[serde(rename = "lang")]
    pub language: String,
    pub kind: String,
    pub category: String,
    pub score: f64,
    pub metrics: RepoBriefMetrics,
    pub why: Vec<String>,
    pub split_hints: Vec<RepoBriefSplitHint>,
    pub next_tools: Vec<RepoBriefNextTool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoBriefMetrics {
    pub line_count: u64,
    pub chunk_count: u64,
    pub symbol_count: u64,
    pub symbol_kinds: BTreeMap<String, u64>,
    pub fan_in: u64,
    pub fan_out: u64,
    pub commit_touch_count: u64,
    pub recent_touch_count: u64,
    pub additions: u64,
    pub deletions: u64,
    pub churn_per_kloc: f64,
    pub github_ref_count: u64,
    pub memories: RepoBriefMemoryCounts,
}

#[derive(Debug, Serialize)]
pub struct RepoBriefSplitHint {
    pub theme: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RepoBriefNextTool {
    pub tool: String,
    pub reason: String,
    pub args: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct FileBriefRow {
    pub(crate) path: String,
    pub(crate) language: String,
    pub(crate) kind: String,
    pub(crate) generated: bool,
    pub(crate) line_count: u64,
    pub(crate) chunk_count: u64,
    pub(crate) symbol_count: u64,
    pub(crate) fan_in: u64,
    pub(crate) fan_out: u64,
    pub(crate) commit_touch_count: u64,
    pub(crate) recent_touch_count: u64,
    pub(crate) additions: u64,
    pub(crate) deletions: u64,
    pub(crate) github_ref_count: u64,
    pub(crate) symbol_kinds: BTreeMap<String, u64>,
    pub(crate) memories: RepoBriefMemoryCounts,
}

pub fn repo_brief(conn: &Connection, options: RepoBriefOptions) -> anyhow::Result<RepoBrief> {
    let mut warnings = Vec::new();
    let summary_counts = summary_counts(conn)?;
    if summary_counts.git_commits == 0 {
        warnings.push("git history is not indexed; churn scores are unavailable".to_string());
    }
    if summary_counts.graph_edges == 0 {
        warnings.push("graph edges are not indexed; coupling scores are unavailable".to_string());
    }

    let mut rows = file_rows(conn, options.include_generated)?;
    if options.include_memories {
        enrich_memory_counts(conn, &mut rows)?;
    }
    rows.sort_by(|a, b| {
        score_for(options.mode, b)
            .total_cmp(&score_for(options.mode, a))
            .then_with(|| b.commit_touch_count.cmp(&a.commit_touch_count))
            .then_with(|| b.symbol_count.cmp(&a.symbol_count))
            .then_with(|| a.path.cmp(&b.path))
    });

    let limit = usize::try_from(options.limit.max(1)).unwrap_or(usize::MAX);
    let enrich_limit = limit.saturating_mul(4).max(limit).min(rows.len());
    enrich_symbol_kinds(conn, &mut rows[..enrich_limit])?;
    let candidates = rows
        .iter()
        .take(limit)
        .map(|row| candidate_for(options.mode, row, options.include_memories))
        .collect::<Vec<_>>();

    Ok(RepoBrief {
        mode: options.mode.as_str().to_string(),
        summary: RepoBriefSummary {
            total_files: summary_counts.total_files,
            returned_files: candidates.len() as u64,
            generated_files: summary_counts.generated_files,
            graph_edges: summary_counts.graph_edges,
            git_commits: summary_counts.git_commits,
            git_file_changes: summary_counts.git_file_changes,
            repo_memories: summary_counts.memories,
            scoring_note: scoring_note(options.mode).to_string(),
        },
        candidates,
        warnings,
    })
}

fn candidate_for(
    mode: RepoBriefMode,
    row: &FileBriefRow,
    include_memories: bool,
) -> RepoBriefCandidate {
    let score = score_for(mode, row);
    let metrics = metrics_for(row);
    let category = category_for(mode, row, &metrics).to_string();
    let why = why_for(mode, row, &metrics, include_memories);
    let split_hints = split_hints_for(mode, row, &metrics);
    let next_tools = next_tools_for(row, include_memories);

    RepoBriefCandidate {
        path: row.path.clone(),
        language: row.language.clone(),
        kind: row.kind.clone(),
        category,
        score,
        metrics,
        why,
        split_hints,
        next_tools,
    }
}

fn metrics_for(row: &FileBriefRow) -> RepoBriefMetrics {
    let churn = row.additions.saturating_add(row.deletions);
    let kloc = (row.line_count as f64 / 1000.0).max(0.001);
    RepoBriefMetrics {
        line_count: row.line_count,
        chunk_count: row.chunk_count,
        symbol_count: row.symbol_count,
        symbol_kinds: row.symbol_kinds.clone(),
        fan_in: row.fan_in,
        fan_out: row.fan_out,
        commit_touch_count: row.commit_touch_count,
        recent_touch_count: row.recent_touch_count,
        additions: row.additions,
        deletions: row.deletions,
        churn_per_kloc: churn as f64 / kloc,
        github_ref_count: row.github_ref_count,
        memories: row.memories.clone(),
    }
}

fn score_for(mode: RepoBriefMode, row: &FileBriefRow) -> f64 {
    let size = capped(row.line_count as f64 / 800.0);
    let symbols = capped(row.symbol_count as f64 / 40.0);
    let coupling = capped((row.fan_in + row.fan_out) as f64 / 50.0);
    let churn = capped(
        capped(row.commit_touch_count as f64 / 25.0) * 0.5
            + capped(row.recent_touch_count as f64 / 8.0) * 0.25
            + capped(row.additions.saturating_add(row.deletions) as f64 / 5000.0) * 0.25,
    );
    let memories = capped(
        capped(row.memories.active as f64 / 5.0) + capped(row.memories.stale as f64 / 3.0) * 0.5,
    );
    let papertrail = capped(row.github_ref_count as f64 / 8.0);

    let score = match mode {
        RepoBriefMode::Spine =>
            coupling * 0.35
                + symbols * 0.18
                + size * 0.12
                + churn * 0.15
                + memories * 0.15
                + papertrail * 0.05,
        RepoBriefMode::Churn =>
            churn * 0.65 + coupling * 0.15 + size * 0.10 + memories * 0.07 + papertrail * 0.03,
        RepoBriefMode::GodModules =>
            size * 0.30 + symbols * 0.25 + coupling * 0.25 + churn * 0.12 + memories * 0.08,
        RepoBriefMode::RefactorCandidates =>
            size * 0.20
                + symbols * 0.18
                + coupling * 0.25
                + churn * 0.25
                + memories * 0.09
                + papertrail * 0.03,
    };

    let score = if row.generated { score * 0.25 } else { score };
    super::round_score(score * support_path_multiplier(mode, &row.path))
}

fn capped(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

fn support_path_multiplier(mode: RepoBriefMode, path: &str) -> f64 {
    if !crate::index::parser::is_test_path(path) {
        return 1.0;
    }
    match mode {
        RepoBriefMode::GodModules | RepoBriefMode::RefactorCandidates => 0.45,
        RepoBriefMode::Spine => 0.70,
        RepoBriefMode::Churn => 1.0,
    }
}

fn category_for(
    mode: RepoBriefMode,
    row: &FileBriefRow,
    metrics: &RepoBriefMetrics,
) -> &'static str {
    match mode {
        RepoBriefMode::Spine =>
            if metrics.fan_in + metrics.fan_out >= 25 {
                "central_coupling_point"
            } else if metrics.memories.active > 0 {
                "source_anchored_context"
            } else {
                "structural_spine"
            },
        RepoBriefMode::Churn =>
            if metrics.recent_touch_count >= 2 {
                "recent_churn_hotspot"
            } else {
                "historical_churn_hotspot"
            },
        RepoBriefMode::GodModules => {
            if row.symbol_count >= 40 && row.fan_in > 0 && row.fan_out > 0 {
                "large_mixed_module"
            } else {
                "large_module_candidate"
            }
        },
        RepoBriefMode::RefactorCandidates => {
            if metrics.commit_touch_count >= 10 && metrics.fan_in + metrics.fan_out >= 15 {
                "high_churn_coupling_point"
            } else {
                "needs_human_review"
            }
        },
    }
}

fn why_for(
    mode: RepoBriefMode,
    row: &FileBriefRow,
    metrics: &RepoBriefMetrics,
    include_memories: bool,
) -> Vec<String> {
    let mut why = Vec::new();
    if metrics.fan_in > 0 || metrics.fan_out > 0 {
        why.push(format!(
            "graph coupling: {} incoming and {} outgoing indexed edges",
            metrics.fan_in, metrics.fan_out
        ));
    }
    if metrics.symbol_count > 0 {
        why.push(format!(
            "source size: {} symbols across {} chunks / {} lines",
            metrics.symbol_count, metrics.chunk_count, metrics.line_count
        ));
    }
    if metrics.commit_touch_count > 0 {
        why.push(format!(
            "churn: touched in {} commits, {} recent; {} additions / {} deletions",
            metrics.commit_touch_count,
            metrics.recent_touch_count,
            metrics.additions,
            metrics.deletions
        ));
    }
    if metrics.github_ref_count > 0 {
        why.push(format!(
            "papertrail density: {} GitHub refs mention this path",
            metrics.github_ref_count
        ));
    }
    if include_memories && (metrics.memories.active > 0 || metrics.memories.stale > 0) {
        why.push(format!(
            "repo memories: {} active and {} stale source-anchored notes",
            metrics.memories.active, metrics.memories.stale
        ));
    }
    if row.generated {
        why.push("generated file: score is downweighted".to_string());
    }
    if why.is_empty() {
        why.push(match mode {
            RepoBriefMode::Spine => "included by baseline indexed source rank".to_string(),
            RepoBriefMode::Churn => "no indexed churn details available for this file".to_string(),
            RepoBriefMode::GodModules | RepoBriefMode::RefactorCandidates =>
                "candidate is weak; inspect before treating it as refactor work".to_string(),
        });
    }
    why
}

fn split_hints_for(
    mode: RepoBriefMode,
    _row: &FileBriefRow,
    metrics: &RepoBriefMetrics,
) -> Vec<RepoBriefSplitHint> {
    if !matches!(mode, RepoBriefMode::GodModules | RepoBriefMode::RefactorCandidates) {
        return Vec::new();
    }

    let mut hints = Vec::new();
    if metrics.symbol_count >= 25 {
        hints.push(RepoBriefSplitHint {
            theme: "symbol clusters".to_string(),
            evidence: vec![format!(
                "{} symbols are indexed in one file; inspect related groups before splitting",
                metrics.symbol_count
            )],
        });
    }
    if metrics.fan_in > 0 && metrics.fan_out > 0 {
        hints.push(RepoBriefSplitHint {
            theme: "boundary extraction".to_string(),
            evidence: vec![format!(
                "file is both depended on and depends outward (fan_in={}, fan_out={})",
                metrics.fan_in, metrics.fan_out
            )],
        });
    }
    if metrics.commit_touch_count >= 10 {
        hints.push(RepoBriefSplitHint {
            theme: "churn-localized helper".to_string(),
            evidence: vec![format!(
                "file changed in {} commits; isolate frequently edited concerns first",
                metrics.commit_touch_count
            )],
        });
    }
    hints
}

fn next_tools_for(row: &FileBriefRow, include_memories: bool) -> Vec<RepoBriefNextTool> {
    // Suggest path-native tools only. `semantic_search` takes a concept query, not a path, so it
    // was misleading here; `impact_surface` text-falls-back on a path and surfaces graph, git,
    // papertrail, and the memories crossing it — the right orientation before editing.
    let mut tools = vec![
        next_tool("git_history_for_path", "inspect churn and rationale commits", [(
            "path",
            row.path.clone(),
        )]),
        next_tool(
            "impact_surface",
            "inspect graph, git, papertrail, and repo memories crossing this path",
            [("query", row.path.clone())],
        ),
    ];
    if include_memories && (row.memories.active > 0 || row.memories.stale > 0) {
        tools.push(next_tool(
            "memory_for_path",
            "read source-anchored repo memories for this path",
            [("path", row.path.clone())],
        ));
    }
    tools
}

fn next_tool<const N: usize>(
    tool: &str,
    reason: &str,
    args: [(&str, String); N],
) -> RepoBriefNextTool {
    RepoBriefNextTool {
        tool: tool.to_string(),
        reason: reason.to_string(),
        args: args
            .into_iter()
            .map(|(key, value)| (key.to_string(), serde_json::Value::String(value)))
            .collect(),
    }
}

fn scoring_note(mode: RepoBriefMode) -> &'static str {
    match mode {
        RepoBriefMode::Spine =>
            "weighted evidence score: graph coupling, symbol/line size, churn, memories, and \
             GitHub refs",
        RepoBriefMode::Churn =>
            "weighted churn score: commit touches, recent touches, additions/deletions, with \
             context signals",
        RepoBriefMode::GodModules =>
            "weighted god-module score: size, symbol count, fan-in/fan-out, churn, and memories",
        RepoBriefMode::RefactorCandidates =>
            "weighted refactor score: churn plus coupling and size; treat as triage evidence, not \
             an automatic refactor order",
    }
}

#[derive(Debug)]
pub(crate) struct SummaryCounts {
    pub(crate) total_files: u64,
    pub(crate) generated_files: u64,
    pub(crate) graph_edges: u64,
    pub(crate) git_commits: u64,
    pub(crate) git_file_changes: u64,
    pub(crate) memories: RepoBriefMemoryCounts,
}

pub(crate) fn summary_counts(conn: &Connection) -> anyhow::Result<SummaryCounts> {
    let (total_files, generated_files) =
        conn.query_row("SELECT COUNT(*), COALESCE(SUM(generated), 0) FROM files", [], |row| {
            Ok((row_u64(row, 0)?, row_u64(row, 1)?))
        })?;
    let graph_edges = table_row_count(conn, "edges")?;
    let git_commits = table_row_count(conn, "git_commits")?;
    let git_file_changes = table_row_count(conn, "git_file_changes")?;
    let memories = memory_counts(conn, None)?;
    Ok(SummaryCounts {
        total_files,
        generated_files,
        graph_edges,
        git_commits,
        git_file_changes,
        memories,
    })
}

pub(crate) fn file_rows(
    conn: &Connection,
    include_generated: bool,
) -> anyhow::Result<Vec<FileBriefRow>> {
    let newest_commit = newest_commit_time(conn)?;
    let recent_floor = newest_commit.saturating_sub(90 * 24 * 60 * 60);
    let mut stmt = conn.prepare(
        "
        WITH file_size AS (
          SELECT file_id,
                 COUNT(*) AS chunk_count,
                 COALESCE(MAX(end_line), 0) AS line_count
          FROM chunks
          GROUP BY file_id
        ),
        symbol_counts AS (
          SELECT file_id, COUNT(*) AS symbol_count
          FROM symbols
          GROUP BY file_id
        ),
        graph_in AS (
          SELECT target_symbols.file_id AS file_id, COUNT(*) AS fan_in
          FROM edges
          JOIN symbols target_symbols ON target_symbols.id = edges.to_symbol_id
          WHERE edges.to_symbol_id IS NOT NULL
          GROUP BY target_symbols.file_id
        ),
        graph_out AS (
          SELECT source_file_id AS file_id, COUNT(*) AS fan_out
          FROM edges
          WHERE source_file_id IS NOT NULL
          GROUP BY source_file_id
        ),
        churn AS (
          SELECT git_file_changes.path,
                 COUNT(DISTINCT git_file_changes.commit_hash) AS commit_touch_count,
                 SUM(CASE WHEN git_commits.authored_at_s >= ?1 THEN 1 ELSE 0 END) AS \
         recent_touch_count,
                 COALESCE(SUM(git_file_changes.additions), 0) AS additions,
                 COALESCE(SUM(git_file_changes.deletions), 0) AS deletions
          FROM git_file_changes
          JOIN git_commits ON git_commits.hash = git_file_changes.commit_hash
          GROUP BY git_file_changes.path
        ),
        github_ref_counts AS (
          SELECT source_path AS path, COUNT(*) AS ref_count
          FROM github_refs
          WHERE source_path IS NOT NULL
          GROUP BY source_path
        )
        SELECT files.path, files.language, files.kind, files.generated,
               COALESCE(file_size.line_count, 0),
               COALESCE(file_size.chunk_count, 0),
               COALESCE(symbol_counts.symbol_count, 0),
               COALESCE(graph_in.fan_in, 0),
               COALESCE(graph_out.fan_out, 0),
               COALESCE(churn.commit_touch_count, 0),
               COALESCE(churn.recent_touch_count, 0),
               COALESCE(churn.additions, 0),
               COALESCE(churn.deletions, 0),
               COALESCE(github_ref_counts.ref_count, 0)
        FROM files
        LEFT JOIN file_size ON file_size.file_id = files.id
        LEFT JOIN symbol_counts ON symbol_counts.file_id = files.id
        LEFT JOIN graph_in ON graph_in.file_id = files.id
        LEFT JOIN graph_out ON graph_out.file_id = files.id
        LEFT JOIN churn ON churn.path = files.path
        LEFT JOIN github_ref_counts ON github_ref_counts.path = files.path
        WHERE (?2 OR files.generated = 0)
        ",
    )?;

    let rows = stmt.query_map((recent_floor, include_generated), |row| {
        Ok(FileBriefRow {
            path: row.get(0)?,
            language: row.get(1)?,
            kind: row.get(2)?,
            generated: row.get::<_, i64>(3)? != 0,
            line_count: row_u64(row, 4)?,
            chunk_count: row_u64(row, 5)?,
            symbol_count: row_u64(row, 6)?,
            fan_in: row_u64(row, 7)?,
            fan_out: row_u64(row, 8)?,
            commit_touch_count: row_u64(row, 9)?,
            recent_touch_count: row_u64(row, 10)?,
            additions: row_u64(row, 11)?,
            deletions: row_u64(row, 12)?,
            github_ref_count: row_u64(row, 13)?,
            symbol_kinds: BTreeMap::new(),
            memories: RepoBriefMemoryCounts::default(),
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub(crate) fn enrich_symbol_kinds(
    conn: &Connection,
    rows: &mut [FileBriefRow],
) -> anyhow::Result<()> {
    enrich_rows(conn, rows, symbol_kind_counts_by_path, |row, value| row.symbol_kinds = value)
}

pub(crate) fn enrich_memory_counts(
    conn: &Connection,
    rows: &mut [FileBriefRow],
) -> anyhow::Result<()> {
    enrich_rows(conn, rows, memory_counts_by_path, |row, value| row.memories = value)
}

/// Shared per-path enrichment: build the `path -> T` map (via `counts_by_path`) once, then `assign`
/// each row its value (defaulting when the path is absent). `enrich_symbol_kinds` /
/// `enrich_memory_counts` were ~identical but for the lookup fn and the target field (#294 dedup).
fn enrich_rows<T: Clone + Default>(
    conn: &Connection,
    rows: &mut [FileBriefRow],
    counts_by_path: impl Fn(&Connection, &[String]) -> anyhow::Result<BTreeMap<String, T>>,
    assign: impl Fn(&mut FileBriefRow, T),
) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let paths = rows.iter().map(|row| row.path.clone()).collect::<Vec<_>>();
    let counts = counts_by_path(conn, &paths)?;
    for row in rows {
        assign(row, counts.get(&row.path).cloned().unwrap_or_default());
    }
    Ok(())
}

fn newest_commit_time(conn: &Connection) -> anyhow::Result<i64> {
    Ok(conn
        .query_row("SELECT MAX(authored_at_s) FROM git_commits", [], |row| row.get(0))
        .optional()?
        .flatten()
        .unwrap_or(0))
}

fn memory_counts(conn: &Connection, path: Option<&str>) -> anyhow::Result<RepoBriefMemoryCounts> {
    let mut counts = RepoBriefMemoryCounts::default();

    if let Some(path) = path {
        let mut stmt = conn.prepare(
            "
            SELECT repo_memories.status, COUNT(DISTINCT repo_memories.id)
            FROM repo_memories
            JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
            WHERE repo_memory_bindings.path = ?1
            GROUP BY repo_memories.status
            ",
        )?;
        let rows =
            stmt.query_map([path], |row| Ok((row.get::<_, String>(0)?, row_u64(row, 1)?)))?;
        for row in rows {
            add_memory_count(&mut counts, row?);
        }
    } else {
        let mut stmt = conn.prepare(
            "
            SELECT status, COUNT(*)
            FROM repo_memories
            GROUP BY status
            ",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row_u64(row, 1)?)))?;
        for row in rows {
            add_memory_count(&mut counts, row?);
        }
    }
    Ok(counts)
}

fn symbol_kind_counts_by_path(
    conn: &Connection,
    paths: &[String],
) -> anyhow::Result<BTreeMap<String, BTreeMap<String, u64>>> {
    if paths.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut counts = BTreeMap::<String, BTreeMap<String, u64>>::new();
    for chunk in paths.chunks(SQL_PARAM_CHUNK) {
        merge_symbol_kind_counts(&mut counts, symbol_kind_counts_for_chunk(conn, chunk)?);
    }
    Ok(counts)
}

fn symbol_kind_counts_for_chunk(
    conn: &Connection,
    paths: &[String],
) -> anyhow::Result<BTreeMap<String, BTreeMap<String, u64>>> {
    let placeholders = repeat_placeholders(paths.len());
    let sql = format!(
        "
        SELECT files.path, symbols.kind, COUNT(*)
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE files.path IN ({placeholders})
        GROUP BY files.path, symbols.kind
        ORDER BY files.path, COUNT(*) DESC, symbols.kind
        "
    );
    let params = paths.iter().map(String::as_str).collect::<Vec<_>>();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row_u64(row, 2)?))
    })?;
    let mut counts = BTreeMap::<String, BTreeMap<String, u64>>::new();
    for row in rows {
        let (path, kind, count) = row?;
        counts.entry(path).or_default().insert(kind, count);
    }
    Ok(counts)
}

fn merge_symbol_kind_counts(
    target: &mut BTreeMap<String, BTreeMap<String, u64>>,
    source: BTreeMap<String, BTreeMap<String, u64>>,
) {
    for (path, counts) in source {
        target.entry(path).or_default().extend(counts);
    }
}

fn memory_counts_by_path(
    conn: &Connection,
    _paths: &[String],
) -> anyhow::Result<BTreeMap<String, RepoBriefMemoryCounts>> {
    let mut stmt = conn.prepare(
        "
        SELECT repo_memory_bindings.path,
               repo_memories.status,
               COUNT(DISTINCT repo_memories.id)
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
        WHERE repo_memory_bindings.path IS NOT NULL
        GROUP BY repo_memory_bindings.path, repo_memories.status
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row_u64(row, 2)?))
    })?;
    let mut counts = BTreeMap::<String, RepoBriefMemoryCounts>::new();
    for row in rows {
        let (path, status, count) = row?;
        add_memory_count(counts.entry(path).or_default(), (status, count));
    }
    Ok(counts)
}

const SQL_PARAM_CHUNK: usize = 500;

fn repeat_placeholders(len: usize) -> String {
    std::iter::repeat_n("?", len).collect::<Vec<_>>().join(",")
}

fn add_memory_count(counts: &mut RepoBriefMemoryCounts, row: (String, u64)) {
    let (status, count) = row;
    match status.as_str() {
        "active" => counts.active += count,
        "stale" => counts.stale += count,
        "obsolete" => counts.obsolete += count,
        _ => counts.other += count,
    }
}

fn row_u64(row: &Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, i64>(idx)?;
    Ok(u64::try_from(value.max(0)).unwrap_or(0))
}
