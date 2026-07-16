use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rag_rat_base::config::Config;
use serde::{Deserialize, Serialize};

use crate::IndexDatabase;
use crate::index::oracle::{OracleEvalMetrics, OracleTool, RecallCalls};
use crate::index::{OracleShaSnapshots, git_history};

const TOP_K: usize = 10;

/// Tool version recorded for the eval oracle pass. Eval consumes pre-built `.scip` fixtures, so the
/// real indexer version isn't known here; a stable label keeps the `(file_sha, tool, tool_version)`
/// staleness key deterministic across eval runs.
const EVAL_ORACLE_TOOL_VERSION: &str = "eval-fixture";

#[derive(Debug, Clone, Deserialize)]
pub struct EvalSuite {
    #[serde(default)]
    pub query: Vec<EvalQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedSuite {
    #[serde(default)]
    pub expected: Vec<ExpectedQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvalQuery {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub evidence_class: Option<String>,
    #[serde(default)]
    pub requires_papertrail_cache: bool,
    #[serde(default)]
    pub must_include_paths: Vec<String>,
    #[serde(default)]
    pub must_include_symbols: Vec<String>,
    #[serde(default)]
    pub must_include_graph_targets: Vec<String>,
    #[serde(default)]
    pub must_include_impact_categories: Vec<String>,
    #[serde(default)]
    pub must_include_impact_paths: Vec<String>,
    #[serde(default)]
    pub must_include_impact_symbols: Vec<String>,
    #[serde(default)]
    pub should_include_git_subjects: Vec<String>,
    #[serde(default)]
    pub should_include_papertrail_kinds: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExpectedQuery {
    pub id: String,
    #[serde(default)]
    pub must_include_paths: Vec<String>,
    #[serde(default)]
    pub must_include_symbols: Vec<String>,
    #[serde(default)]
    pub must_include_graph_targets: Vec<String>,
    #[serde(default)]
    pub must_include_impact_categories: Vec<String>,
    #[serde(default)]
    pub must_include_impact_paths: Vec<String>,
    #[serde(default)]
    pub must_include_impact_symbols: Vec<String>,
    #[serde(default)]
    pub should_include_git_subjects: Vec<String>,
    #[serde(default)]
    pub should_include_papertrail_kinds: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EvalOptions {
    pub queries_path: PathBuf,
    pub expected_path: PathBuf,
    pub update_baseline: bool,
    /// Optional pre-built `.scip` to drive the SCIP-oracle precision/recall metrics against the
    /// eval corpus. `None` (or a missing path) skips the oracle pass — `EvalReport.oracle` is then
    /// `None` and oracle metrics aren't gated.
    pub scip_path: Option<PathBuf>,
    /// Commit-replay mode (#120): instead of the hand-authored TOML suite, generate eval cases
    /// from the indexed git history (commit message = query, diff's changed paths = recall
    /// gold). `None` uses the static `queries_path` suite.
    pub replay: Option<ReplayOptions>,
    /// Run searches with the graded-git rerank ON (#109, `--rerank`): scores the SAME at-head
    /// index with `SearchOptions::graded_history = true`. Applied to BOTH the active AND the
    /// hash-vector-baseline search so the delta block compares reranked-vs-reranked (same axis),
    /// never reranked-vs-unreranked. Default false → today's fuse.
    pub rerank: bool,
    /// How many hits each `db.search` returns — the width of the candidate pool the eval scores
    /// (#109, `--search-limit`). Default [`TOP_K`] (=10), so a plain run is unchanged. The fixed
    /// `recall_at_3`/`recall_at_10` cutoffs are independent of this (they slice the first 3/10
    /// hits); widening it only grows `recall_at_returned`. At 100 it measures recall@100 ≈ the
    /// candidate-generation ceiling (of the gold, what fraction appears ANYWHERE in a wide set),
    /// which separates the ranking lever from the generation lever — no search behavior change.
    pub search_limit: usize,
}

/// Commit-replay eval knobs (#120).
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Cap on how many recent commits become eval cases.
    pub max_cases: u32,
    /// Drop bulk/mechanical commits whose `changed_file_count` exceeds this (recall noise).
    pub max_files: u32,
}

#[derive(Debug, Serialize)]
pub struct EvalReport {
    pub pass: bool,
    pub queries: usize,
    pub metrics: EvalMetrics,
    pub hash_vector_baseline: EvalBaselineReport,
    /// SCIP-oracle precision/recall metrics (#68), present only when a `.scip` fixture was
    /// supplied and the oracle pass ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oracle: Option<OracleEvalMetrics>,
    /// Edge candidates the oracle pass skipped because their source file drifted between the index
    /// build and the `.scip` (`file_sha` mismatch — content-integrity, #81 finding 2). Non-zero
    /// means some documents were out of sync and their verdicts were correctly withheld; `eval`
    /// surfaces it so the recall/precision numbers aren't silently computed over drifted content.
    #[serde(skip_serializing_if = "is_zero")]
    pub oracle_skipped_drifted: u64,
    pub results: Vec<EvalQueryReport>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Serialize)]
pub struct EvalBaselineReport {
    pub model_id: String,
    pub available: bool,
    pub current_artifacts: u64,
    pub metrics: EvalMetrics,
    pub delta_mrr_at_10: f64,
    pub delta_recall_at_10: f64,
    pub delta_recall_at_3: f64,
    pub delta_recall_at_returned: f64,
    pub delta_path_hit_rate: f64,
    pub delta_symbol_hit_rate: f64,
}

#[derive(Debug, Serialize)]
pub struct EvalMetrics {
    pub mrr_at_10: f64,
    pub recall_at_10: f64,
    pub recall_at_3: f64,
    /// Path/symbol recall membership (same predicates as `recall_at_10`) measured over the ENTIRE
    /// returned `hits` slice rather than a fixed cutoff — the candidate-recall ceiling for the
    /// configured `search_limit`. `recall_at_10 <= recall_at_returned` always (top-10 ⊆ returned);
    /// at `--search-limit 100` this is recall@100 ≈ the generation ceiling (#109).
    pub recall_at_returned: f64,
    pub path_hit_rate: f64,
    pub symbol_hit_rate: f64,
    pub graph_evidence_hit_rate: f64,
    pub impact_hit_rate: f64,
    pub git_evidence_hit_rate: f64,
    pub papertrail_evidence_hit_rate: f64,
    pub stale_hit_rate: f64,
    pub stale_current_source_violations: u64,
    pub current_source_violation_count: u64,
    pub papertrail_precision_sample: Option<f64>,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct EvalQueryReport {
    pub id: String,
    pub text: String,
    pub passed: bool,
    pub skipped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    pub reciprocal_rank_at_10: f64,
    pub recall_at_10: f64,
    pub recall_at_3: f64,
    /// Same path/symbol recall membership as `recall_at_10`, but over the WHOLE returned `hits`
    /// slice (no fixed cutoff). `recall_at_10 <= recall_at_returned`; the ceiling at a wide
    /// `search_limit` (#109).
    pub recall_at_returned: f64,
    pub path_hits: Vec<String>,
    pub missing_paths: Vec<String>,
    pub symbol_hits: Vec<String>,
    pub missing_symbols: Vec<String>,
    pub graph_target_hits: Vec<String>,
    pub missing_graph_targets: Vec<String>,
    pub impact_category_hits: Vec<String>,
    pub missing_impact_categories: Vec<String>,
    pub impact_path_hits: Vec<String>,
    pub missing_impact_paths: Vec<String>,
    pub impact_symbol_hits: Vec<String>,
    pub missing_impact_symbols: Vec<String>,
    pub git_subject_hits: Vec<String>,
    pub missing_git_subjects: Vec<String>,
    pub papertrail_kind_hits: Vec<String>,
    pub missing_papertrail_kinds: Vec<String>,
    pub papertrail_precision_sample: Option<f64>,
    pub stale_current_source_violations: u64,
    pub current_source_violations: Vec<CurrentSourceViolation>,
    pub latency_ms: f64,
    pub top_hits: Vec<EvalSearchHit>,
}

#[derive(Debug, Serialize)]
pub struct EvalSearchHit {
    pub rank: usize,
    pub chunk_id: i64,
    pub path: String,
    pub symbol_path: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    pub score: f64,
}

#[derive(Debug, Serialize)]
pub struct CurrentSourceViolation {
    pub chunk_id: i64,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
struct BaselineSuite {
    expected: Vec<ExpectedQuery>,
}

pub fn run(config: &Config, options: &EvalOptions) -> anyhow::Result<EvalReport> {
    let db = IndexDatabase::open_config(config)?;
    let suite = match &options.replay {
        Some(replay) => generate_replay_suite(&db, replay)?,
        None => load_queries(&options.queries_path)?,
    };
    let expected = load_expected(&options.expected_path)?;
    let mut results = Vec::new();
    let mut observed = Vec::new();

    for query in &suite.query {
        let expected_query = expected.get(&query.id);
        let merged = merge_expected(query.clone(), expected_query);
        let report = evaluate_query(
            config,
            &db,
            &merged,
            SearchMode::Active,
            options.rerank,
            options.search_limit,
        )?;
        observed.push(observed_expected(&report));
        results.push(report);
    }

    if options.update_baseline {
        write_baseline(&options.expected_path, observed)?;
    }

    let metrics = aggregate(&results);
    let baseline = hash_vector_baseline(
        config,
        &db,
        &suite.query,
        &expected,
        &metrics,
        options.rerank,
        options.search_limit,
    )?;
    let oracle_eval = run_oracle_eval(&db, options)?;
    let (oracle, oracle_skipped_drifted) = match oracle_eval {
        Some((metrics, skipped_drifted)) => (Some(metrics), skipped_drifted),
        None => (None, 0),
    };
    // Commit-replay is a MEASUREMENT (the aggregate MRR/recall@10 IS the output), not a hard
    // must-pass gate — a commit whose files don't all land in top-10 is exactly what recall
    // measures, not a failure. So in replay mode the run still "passes" as long as content
    // integrity holds (no stale-source violations); the static hand-authored suite keeps its
    // strict all-must-include regression gate.
    let pass = metrics.stale_current_source_violations == 0
        && (options.replay.is_some() || results.iter().all(|r| r.passed));
    Ok(EvalReport {
        pass,
        queries: results.len(),
        metrics,
        hash_vector_baseline: baseline,
        oracle,
        oracle_skipped_drifted,
        results,
    })
}

/// Map one commit-replay case to the standard `EvalQuery`: commit message = query, the diff's
/// changed paths = recall gold. Shared by HEAD-scored ([`generate_replay_suite`]) and parent-state
/// ([`run_replay_parent_state`]) replay so both score identically.
fn replay_eval_query(case: &git_history::ReplayCase) -> EvalQuery {
    let text = if case.body.trim().is_empty() {
        case.subject.clone()
    } else {
        format!("{}\n{}", case.subject, case.body)
    };
    let short = &case.hash[..case.hash.len().min(12)];
    EvalQuery {
        id: format!("replay-{short}"),
        text,
        evidence_class: None,
        requires_papertrail_cache: false,
        must_include_paths: case.changed_paths.clone(),
        must_include_symbols: Vec::new(),
        must_include_graph_targets: Vec::new(),
        must_include_impact_categories: Vec::new(),
        must_include_impact_paths: Vec::new(),
        must_include_impact_symbols: Vec::new(),
        should_include_git_subjects: Vec::new(),
        should_include_papertrail_kinds: Vec::new(),
    }
}

/// Generate commit-replay eval cases (#120) from the indexed git history. Cases flow through the
/// standard `EvalQuery` scoring path, so MRR/recall@10 are computed identically to the
/// hand-authored suite.
///
/// This is the HEAD-SCORED variant: every case is scored against the currently-open (HEAD) index,
/// not a checkout of each commit's parent — so absolute numbers carry leakage (a post-fix index can
/// name the solution). Relative deltas (embedder A vs B, reranker on vs off) are unaffected — the
/// immediate need for #112/#109. Use [`run_replay_parent_state`] for the leakage-free number.
pub fn generate_replay_suite(
    db: &IndexDatabase,
    options: &ReplayOptions,
) -> anyhow::Result<EvalSuite> {
    let cases = db.replay_commit_cases(options.max_cases, options.max_files)?;
    let indexed = db.indexed_path_set()?;
    Ok(EvalSuite { query: replay_queries_with_indexed_gold(&cases, &indexed) })
}

/// Turn replay cases into eval queries, restricting each case's path gold to paths the index
/// actually contains. The repo config indexes only a subset of the tree, so a commit touching
/// `.github/**`, `tools/**`, or a root manifest contributes gold that can never be retrieved;
/// counting it as a miss would make recall track the file mix of recent commits, not search quality
/// (#315). Cases left with no indexed gold are dropped — nothing measurable remains.
fn replay_queries_with_indexed_gold(
    cases: &[git_history::ReplayCase],
    indexed: &BTreeSet<String>,
) -> Vec<EvalQuery> {
    cases
        .iter()
        .filter_map(|case| {
            let mut query = replay_eval_query(case);
            query.must_include_paths.retain(|path| indexed.contains(path));
            (!query.must_include_paths.is_empty()).then_some(query)
        })
        .collect()
}

/// Aggregate result of a parent-state replay run (#120) — the leakage-free MRR/recall@10 over the
/// commits that were scorable, plus how many were skipped (root commit / pre-`rag-rat.toml`).
#[derive(Debug, Serialize)]
pub struct ReplayReport {
    pub cases: usize,
    pub skipped: u32,
    pub parent_state: bool,
    pub metrics: EvalMetrics,
    pub results: Vec<EvalQueryReport>,
}

/// Leakage-free commit-replay (#120): score each commit's query against an index of its PARENT
/// state — the code as it was BEFORE the change — so a post-fix index can't leak the answer. Each
/// case checks out the parent in a throwaway `git worktree`, full-indexes it into a temp DB, and
/// scores via the same `evaluate_query` path. Slower than HEAD-scoring (one full index per case),
/// so it's a separate opt-in mode (`--replay-parent-state`), for the absolute headline number
/// rather than the inner-loop relative dial.
pub fn run_replay_parent_state(
    config: &Config,
    options: &ReplayOptions,
) -> anyhow::Result<ReplayReport> {
    let cases = {
        let head_db = IndexDatabase::open_config(config)?;
        head_db.replay_commit_cases(options.max_cases, options.max_files)?
        // head_db dropped here — release the HEAD index before rebuilding worktree indexes.
    };
    let mut results = Vec::new();
    let mut skipped = 0u32;
    for case in &cases {
        match score_case_at_parent(&config.root, case) {
            Ok(Some(report)) => results.push(report),
            Ok(None) => skipped += 1, // root commit (no parent) or pre-`rag-rat.toml` history
            Err(err) => {
                eprintln!("replay parent-state: skipped {} ({err})", case.hash);
                skipped += 1;
            },
        }
    }
    let metrics = aggregate(&results);
    Ok(ReplayReport { cases: results.len(), skipped, parent_state: true, metrics, results })
}

/// Index a single commit's PARENT state in a throwaway worktree and score its replay query there.
/// `Ok(None)` = legitimately skippable (no parent / no `rag-rat.toml` at that commit).
fn score_case_at_parent(
    repo_root: &Path,
    case: &git_history::ReplayCase,
) -> anyhow::Result<Option<EvalQueryReport>> {
    let short = &case.hash[..case.hash.len().min(12)];
    // The root commit has no parent — `git worktree add <sha>^` fails; that's a skip, not an error.
    let Ok(worktree) = ParentWorktree::create(repo_root, &format!("{}^", case.hash), short) else {
        return Ok(None);
    };
    let manifest = worktree.path.join("rag-rat.toml");
    if !manifest.exists() {
        return Ok(None); // commit predates rag-rat.toml — nothing to index against.
    }
    // At the PARENT state, files the commit ADDED don't exist yet — they can't be retrieved, and
    // requiring them would understate recall. Restrict the gold to paths that existed at the parent
    // (modified/deleted); skip commits that were pure additions (nothing recallable to score).
    let existing_gold: Vec<String> = case
        .changed_paths
        .iter()
        .filter(|path| worktree.path.join(path).exists())
        .cloned()
        .collect();
    if existing_gold.is_empty() {
        return Ok(None);
    }
    let mut case_config = Config::load(&manifest)?;
    case_config.database = std::env::temp_dir().join(format!("rag-rat-replay-{short}.sqlite"));
    let _ = std::fs::remove_file(&case_config.database);
    IndexDatabase::rebuild(&case_config)?;
    let report = {
        let case_db = IndexDatabase::open_config(&case_config)?;
        let mut query = replay_eval_query(case);
        // Symbol-level gold: the symbols the commit touched that existed at the parent. Derive from
        // the PARENT-side diff line ranges → the parent index's chunk `symbol_path`s (same format
        // the search hits carry), so symbol-recall is measured alongside path-recall.
        let parent = format!("{}^", case.hash);
        let mut symbol_gold = BTreeSet::new();
        for path in &existing_gold {
            let ranges = parent_changed_line_ranges(repo_root, &parent, &case.hash, path);
            for symbol in case_db.chunk_symbol_paths_in_ranges(path, &ranges)? {
                symbol_gold.insert(symbol);
            }
        }
        query.must_include_paths = existing_gold;
        query.must_include_symbols = symbol_gold.into_iter().collect();
        // Parent-state replay is the leakage-free HEADLINE number, not the reranker A/B dial (that
        // is HEAD-scored `--replay --rerank`); score it with the default fuse (rerank off) and the
        // default `TOP_K` candidate width (the candidate-ceiling dial is HEAD-scored).
        evaluate_query(&case_config, &case_db, &query, SearchMode::Active, false, TOP_K)?
    };
    let _ = std::fs::remove_file(&case_config.database);
    Ok(Some(report))
}

/// PARENT-side changed line ranges (inclusive) for `path` between a `commit` and its `parent`, from
/// `git diff --unified=0`. Only hunks with parent-side lines (old-count > 0) are returned —
/// pure-add hunks have no lines that existed at the parent. Feeds symbol-level replay gold (#120).
fn parent_changed_line_ranges(
    repo_root: &Path,
    parent: &str,
    commit: &str,
    path: &str,
) -> Vec<(i64, i64)> {
    let Ok(output) = std::process::Command::new("git")
        .current_dir(repo_root)
        .args(["diff", "--unified=0", &format!("{parent}..{commit}"), "--", path])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut ranges = Vec::new();
    for line in text.lines() {
        // Hunk header: `@@ -OLD_START[,OLD_COUNT] +NEW_START[,NEW_COUNT] @@ ...`
        let Some(rest) = line.strip_prefix("@@ -") else {
            continue;
        };
        let old = rest.split_whitespace().next().unwrap_or("");
        let mut parts = old.split(',');
        let start: i64 = parts.next().and_then(|value| value.parse().ok()).unwrap_or(0);
        let count: i64 = parts.next().map_or(1, |value| value.parse().unwrap_or(1));
        if start > 0 && count > 0 {
            ranges.push((start, start + count - 1));
        }
    }
    ranges
}

/// RAII throwaway `git worktree` at a commit-ish. Created detached; removed (force) on drop so a
/// scoring error never leaks a worktree. Pruned/cleared first so a stale dir from a crashed run
/// doesn't block re-creation.
struct ParentWorktree {
    path: PathBuf,
    repo_root: PathBuf,
}

/// `git` invocation with rag-rat's own git hooks suppressed (`RAG_RAT_HOOK_DISABLE=1`). The
/// parent-state replay creates and tears down a throwaway worktree PER CASE; without this each
/// `git worktree add` fires the post-checkout hook → a `rag-rat maintenance` pass on the throwaway
/// worktree, i.e. N redundant heavy index passes per replay run (real memory pressure). The replay
/// indexes the worktree explicitly anyway, so the hook has nothing useful to add.
fn git_without_hooks(repo_root: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command.current_dir(repo_root).env("RAG_RAT_HOOK_DISABLE", "1");
    command
}

impl ParentWorktree {
    fn create(repo_root: &Path, commitish: &str, dir_key: &str) -> anyhow::Result<Self> {
        let path = std::env::temp_dir().join(format!("rag-rat-replay-wt-{dir_key}"));
        let _ = git_without_hooks(repo_root).args(["worktree", "prune"]).status();
        let _ = std::fs::remove_dir_all(&path);
        let status = git_without_hooks(repo_root)
            .args(["worktree", "add", "--detach"])
            .arg(&path)
            .arg(commitish)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            anyhow::bail!("git worktree add failed for {commitish}");
        }
        Ok(Self { path, repo_root: repo_root.to_path_buf() })
    }
}

impl Drop for ParentWorktree {
    fn drop(&mut self) {
        let _ = git_without_hooks(&self.repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Run the SCIP-oracle pass against the eval corpus from the supplied `.scip` fixture, returning
/// its heuristic-vs-oracle metrics plus the count of candidates skipped for content drift. `None`
/// when no fixture path is configured or the file is absent — the oracle is opt-in and never an
/// error when unavailable (mirrors the missing-embedding degradation). The pass writes
/// `edge_oracle` side rows; the heuristic `edges` rows are untouched.
fn run_oracle_eval(
    db: &IndexDatabase,
    options: &EvalOptions,
) -> anyhow::Result<Option<(OracleEvalMetrics, u64)>> {
    let Some(scip_path) = options.scip_path.as_ref() else {
        return Ok(None);
    };
    if !scip_path.exists() {
        return Ok(None);
    }
    let scip_bytes = fs::read(scip_path).map_err(|err| {
        anyhow::anyhow!("failed to read SCIP fixture {}: {err}", scip_path.display())
    })?;
    // Eval consumes a pre-built fixture `.scip` (no tool subprocess), so there is no production
    // snapshot — `None` leaves only the index-vs-disk content gate, as on the `--scip` CLI path.
    let report = db.run_oracle(
        OracleTool::RustAnalyzer,
        EVAL_ORACLE_TOOL_VERSION,
        &scip_bytes,
        OracleShaSnapshots::default(),
    )?;
    // Both recall sides come from the run, occurrence-counted over the call population.
    let recall_calls =
        RecallCalls { covered: report.covered_calls, oracle_only: report.oracle_only_calls };
    let metrics =
        db.oracle_eval_metrics(OracleTool::RustAnalyzer, EVAL_ORACLE_TOOL_VERSION, recall_calls)?;
    Ok(Some((metrics, report.skipped_drifted)))
}

fn load_queries(path: &Path) -> anyhow::Result<EvalSuite> {
    let text = fs::read_to_string(path)
        .map_err(|err| anyhow::anyhow!("failed to read eval queries {}: {err}", path.display()))?;
    toml::from_str(&text)
        .map_err(|err| anyhow::anyhow!("failed to parse eval queries {}: {err}", path.display()))
}

fn load_expected(path: &Path) -> anyhow::Result<BTreeMap<String, ExpectedQuery>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = fs::read_to_string(path).map_err(|err| {
        anyhow::anyhow!("failed to read eval expected hits {}: {err}", path.display())
    })?;
    let suite: ExpectedSuite = toml::from_str(&text).map_err(|err| {
        anyhow::anyhow!("failed to parse eval expected hits {}: {err}", path.display())
    })?;
    Ok(suite.expected.into_iter().map(|expected| (expected.id.clone(), expected)).collect())
}

fn merge_expected(query: EvalQuery, expected: Option<&ExpectedQuery>) -> EvalQuery {
    let Some(expected) = expected else {
        return query;
    };
    EvalQuery {
        id: query.id,
        text: query.text,
        evidence_class: query.evidence_class,
        requires_papertrail_cache: query.requires_papertrail_cache,
        must_include_paths: union(query.must_include_paths, &expected.must_include_paths),
        must_include_symbols: union(query.must_include_symbols, &expected.must_include_symbols),
        must_include_graph_targets: union(
            query.must_include_graph_targets,
            &expected.must_include_graph_targets,
        ),
        must_include_impact_categories: union(
            query.must_include_impact_categories,
            &expected.must_include_impact_categories,
        ),
        must_include_impact_paths: union(
            query.must_include_impact_paths,
            &expected.must_include_impact_paths,
        ),
        must_include_impact_symbols: union(
            query.must_include_impact_symbols,
            &expected.must_include_impact_symbols,
        ),
        should_include_git_subjects: union(
            query.should_include_git_subjects,
            &expected.should_include_git_subjects,
        ),
        should_include_papertrail_kinds: union(
            query.should_include_papertrail_kinds,
            &expected.should_include_papertrail_kinds,
        ),
    }
}

fn union(mut values: Vec<String>, extra: &[String]) -> Vec<String> {
    let mut seen = values.iter().cloned().collect::<BTreeSet<_>>();
    for value in extra {
        if seen.insert(value.clone()) {
            values.push(value.clone());
        }
    }
    values
}

fn evaluate_query(
    config: &Config,
    db: &IndexDatabase,
    query: &EvalQuery,
    mode: SearchMode,
    rerank: bool,
    search_limit: usize,
) -> anyhow::Result<EvalQueryReport> {
    if query.requires_papertrail_cache && !papertrail_cache_available(db)? {
        return Ok(skipped_report(
            query,
            "papertrail cache is empty; run `rag-rat papertrail sync`",
        ));
    }

    let started = Instant::now();
    let mut hits = search(db, mode, &query.text, rerank, search_limit)?;
    let mut latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut current_source_violations = find_current_source_violations(config, db, &hits)?;
    if !current_source_violations.is_empty() {
        let retry_started = Instant::now();
        hits = search(db, mode, &query.text, rerank, search_limit)?;
        latency_ms += retry_started.elapsed().as_secs_f64() * 1000.0;
        current_source_violations = find_current_source_violations(config, db, &hits)?;
    }
    let top_hits = top_hits(&hits);

    let path_hits = query
        .must_include_paths
        .iter()
        .filter(|expected| hits.iter().any(|hit| hit.path == **expected))
        .cloned()
        .collect::<Vec<_>>();
    let missing_paths = missing(&query.must_include_paths, &path_hits);
    let symbol_hits = query
        .must_include_symbols
        .iter()
        .filter(|expected| {
            hits.iter()
                .filter_map(|hit| hit.symbol_path.as_deref())
                .any(|symbol| symbol == expected.as_str() || symbol.ends_with(expected.as_str()))
        })
        .cloned()
        .collect::<Vec<_>>();
    let missing_symbols = missing(&query.must_include_symbols, &symbol_hits);

    let graph_target_hits = query
        .must_include_graph_targets
        .iter()
        .filter(|expected| hits.iter().any(|hit| graph_hit_matches(hit, expected)))
        .cloned()
        .collect::<Vec<_>>();
    let missing_graph_targets = missing(&query.must_include_graph_targets, &graph_target_hits);

    let impact = if query.must_include_impact_categories.is_empty()
        && query.must_include_impact_paths.is_empty()
        && query.must_include_impact_symbols.is_empty()
    {
        Vec::new()
    } else {
        db.impact_surface(&query.text, TOP_K as u32).unwrap_or_default()
    };
    let impact_category_hits = query
        .must_include_impact_categories
        .iter()
        .filter(|expected| impact.iter().any(|item| item.category == **expected))
        .cloned()
        .collect::<Vec<_>>();
    let missing_impact_categories =
        missing(&query.must_include_impact_categories, &impact_category_hits);
    let impact_path_hits = query
        .must_include_impact_paths
        .iter()
        .filter(|expected| impact.iter().any(|item| item.path == **expected))
        .cloned()
        .collect::<Vec<_>>();
    let missing_impact_paths = missing(&query.must_include_impact_paths, &impact_path_hits);
    let impact_symbol_hits = query
        .must_include_impact_symbols
        .iter()
        .filter(|expected| {
            impact
                .iter()
                .filter_map(|item| item.symbol.as_deref())
                .any(|symbol| symbol == expected.as_str() || symbol.ends_with(expected.as_str()))
        })
        .cloned()
        .collect::<Vec<_>>();
    let missing_impact_symbols = missing(&query.must_include_impact_symbols, &impact_symbol_hits);

    let commit_hits = db.commit_search(&query.text, TOP_K as u32).unwrap_or_default();
    let git_subject_hits = query
        .should_include_git_subjects
        .iter()
        .filter(|expected| {
            let needle = expected.to_ascii_lowercase();
            commit_hits.iter().any(|hit| hit.subject.to_ascii_lowercase().contains(&needle))
        })
        .cloned()
        .collect::<Vec<_>>();
    let missing_git_subjects = missing(&query.should_include_git_subjects, &git_subject_hits);

    let papertrail = db.rationale_search(&query.text, TOP_K as u32).unwrap_or_default();
    let papertrail_kind_hits = query
        .should_include_papertrail_kinds
        .iter()
        .filter(|expected| {
            let needle = normalize_kind(expected);
            papertrail.iter().any(|item| normalize_kind(&item.classification) == needle)
        })
        .cloned()
        .collect::<Vec<_>>();
    let missing_papertrail_kinds =
        missing(&query.should_include_papertrail_kinds, &papertrail_kind_hits);
    let papertrail_precision_sample = if query.should_include_papertrail_kinds.is_empty() {
        None
    } else if papertrail.is_empty() {
        Some(0.0)
    } else {
        let expected = query
            .should_include_papertrail_kinds
            .iter()
            .map(|kind| normalize_kind(kind))
            .collect::<BTreeSet<_>>();
        let matched = papertrail
            .iter()
            .filter(|item| expected.contains(&normalize_kind(&item.classification)))
            .count();
        Some(matched as f64 / papertrail.len() as f64)
    };

    let stale_current_source_violations =
        u64::try_from(current_source_violations.len()).unwrap_or(u64::MAX);
    let relevant_rank = hits.iter().position(|hit| relevant(hit, query)).map(|rank| rank + 1);
    let reciprocal_rank_at_10 = relevant_rank.map(|rank| 1.0 / rank as f64).unwrap_or(0.0);
    let expected_relevant = query.must_include_paths.len() + query.must_include_symbols.len();
    // `path_hits`/`symbol_hits` test membership over the WHOLE returned `hits` slice, so this is
    // the candidate-recall ceiling for the configured `search_limit` (recall over the full
    // returned list, no fixed cutoff). At `search_limit = TOP_K` it equals `recall_at_10`
    // below; widening the limit can only grow it, so `recall_at_10 <= recall_at_returned`
    // always holds.
    let found_relevant = path_hits.len() + symbol_hits.len();
    let recall_at_returned =
        if expected_relevant == 0 { 1.0 } else { found_relevant as f64 / expected_relevant as f64 };
    // recall@10 fixes the cutoff at the first 10 hits regardless of `search_limit` — slicing the
    // SAME membership predicates over `hits[..10]` keeps its meaning stable even at a wide limit.
    let top10 = &hits[..TOP_K.min(hits.len())];
    let found_relevant_at_10 = query
        .must_include_paths
        .iter()
        .filter(|expected| top10.iter().any(|hit| hit.path == **expected))
        .count()
        + query
            .must_include_symbols
            .iter()
            .filter(|expected| {
                top10.iter().filter_map(|hit| hit.symbol_path.as_deref()).any(|symbol| {
                    symbol == expected.as_str() || symbol.ends_with(expected.as_str())
                })
            })
            .count();
    let recall_at_10 = if expected_relevant == 0 {
        1.0
    } else {
        found_relevant_at_10 as f64 / expected_relevant as f64
    };
    // recall@3 reuses the identical path/symbol membership predicates as recall@10 above; the only
    // difference is the slice — membership is tested over the first 3 hits, not the full top-10.
    // A within-top-10 reorder (the reranker A/B target) moves this but not recall@10.
    let top3 = &hits[..3.min(hits.len())];
    let found_relevant_at_3 = query
        .must_include_paths
        .iter()
        .filter(|expected| top3.iter().any(|hit| hit.path == **expected))
        .count()
        + query
            .must_include_symbols
            .iter()
            .filter(|expected| {
                top3.iter().filter_map(|hit| hit.symbol_path.as_deref()).any(|symbol| {
                    symbol == expected.as_str() || symbol.ends_with(expected.as_str())
                })
            })
            .count();
    let recall_at_3 = if expected_relevant == 0 {
        1.0
    } else {
        found_relevant_at_3 as f64 / expected_relevant as f64
    };
    let passed = stale_current_source_violations == 0
        && missing_paths.is_empty()
        && missing_symbols.is_empty()
        && missing_graph_targets.is_empty()
        && missing_impact_categories.is_empty()
        && missing_impact_paths.is_empty()
        && missing_impact_symbols.is_empty()
        && missing_git_subjects.is_empty()
        && missing_papertrail_kinds.is_empty();

    Ok(EvalQueryReport {
        id: query.id.clone(),
        text: query.text.clone(),
        passed,
        skipped: false,
        skip_reason: None,
        reciprocal_rank_at_10,
        recall_at_10,
        recall_at_3,
        recall_at_returned,
        path_hits,
        missing_paths,
        symbol_hits,
        missing_symbols,
        graph_target_hits,
        missing_graph_targets,
        impact_category_hits,
        missing_impact_categories,
        impact_path_hits,
        missing_impact_paths,
        impact_symbol_hits,
        missing_impact_symbols,
        git_subject_hits,
        missing_git_subjects,
        papertrail_kind_hits,
        missing_papertrail_kinds,
        papertrail_precision_sample,
        stale_current_source_violations,
        current_source_violations,
        latency_ms,
        top_hits,
    })
}

fn skipped_report(query: &EvalQuery, reason: impl Into<String>) -> EvalQueryReport {
    EvalQueryReport {
        id: query.id.clone(),
        text: query.text.clone(),
        passed: true,
        skipped: true,
        skip_reason: Some(reason.into()),
        reciprocal_rank_at_10: 0.0,
        recall_at_10: 1.0,
        recall_at_3: 1.0,
        recall_at_returned: 1.0,
        path_hits: Vec::new(),
        missing_paths: Vec::new(),
        symbol_hits: Vec::new(),
        missing_symbols: Vec::new(),
        graph_target_hits: Vec::new(),
        missing_graph_targets: Vec::new(),
        impact_category_hits: Vec::new(),
        missing_impact_categories: Vec::new(),
        impact_path_hits: Vec::new(),
        missing_impact_paths: Vec::new(),
        impact_symbol_hits: Vec::new(),
        missing_impact_symbols: Vec::new(),
        git_subject_hits: Vec::new(),
        missing_git_subjects: Vec::new(),
        papertrail_kind_hits: Vec::new(),
        missing_papertrail_kinds: Vec::new(),
        papertrail_precision_sample: None,
        stale_current_source_violations: 0,
        current_source_violations: Vec::new(),
        latency_ms: 0.0,
        top_hits: Vec::new(),
    }
}

fn papertrail_cache_available(db: &IndexDatabase) -> anyhow::Result<bool> {
    let status = db.papertrail_sync_status()?;
    Ok(status.issues + status.change_requests + status.comments > 0)
}

#[derive(Debug, Clone, Copy)]
enum SearchMode {
    Active,
    HashBaseline,
}

fn search(
    db: &IndexDatabase,
    mode: SearchMode,
    query: &str,
    rerank: bool,
    search_limit: usize,
) -> anyhow::Result<Vec<crate::search::lexical::SearchHit>> {
    // `rerank` flows identically into BOTH search modes so the active-vs-baseline delta compares
    // the same reranker axis (#109). The active path threads it through
    // `SearchRequest.options`; the hash baseline takes it directly. `search_limit` is the width of
    // the candidate pool both modes return (default `TOP_K`); it never changes the fixed
    // `recall_at_3`/`recall_at_10` cutoffs — only the `recall_at_returned` ceiling.
    let limit = u32::try_from(search_limit).unwrap_or(u32::MAX);
    match mode {
        SearchMode::Active => db.search_with_graph_meta(crate::index::SearchRequest {
            include_generated: false,
            options: crate::search::lexical::SearchOptions {
                graded_history: rerank,
                ..crate::search::lexical::SearchOptions::default()
            },
            ..crate::index::SearchRequest::new(query, limit)
        }),
        SearchMode::HashBaseline => db.search_hash_baseline(query, limit, false, rerank),
    }
}

fn hash_vector_baseline(
    config: &Config,
    db: &IndexDatabase,
    queries: &[EvalQuery],
    expected: &BTreeMap<String, ExpectedQuery>,
    active_metrics: &EvalMetrics,
    rerank: bool,
    search_limit: usize,
) -> anyhow::Result<EvalBaselineReport> {
    let mut results = Vec::new();
    for query in queries {
        let merged = merge_expected(query.clone(), expected.get(&query.id));
        // SAME `rerank` value AND `search_limit` as the active pass so the delta block compares the
        // same axes (#109).
        results.push(evaluate_query(
            config,
            db,
            &merged,
            SearchMode::HashBaseline,
            rerank,
            search_limit,
        )?);
    }
    let metrics = aggregate(&results);
    let current_artifacts =
        db.current_embedding_count(rag_rat_base::embedding_models::HASH_MODEL_ID)?;
    Ok(EvalBaselineReport {
        model_id: rag_rat_base::embedding_models::HASH_MODEL_ID.to_string(),
        available: current_artifacts > 0,
        current_artifacts,
        delta_mrr_at_10: active_metrics.mrr_at_10 - metrics.mrr_at_10,
        delta_recall_at_10: active_metrics.recall_at_10 - metrics.recall_at_10,
        delta_recall_at_3: active_metrics.recall_at_3 - metrics.recall_at_3,
        delta_recall_at_returned: active_metrics.recall_at_returned - metrics.recall_at_returned,
        delta_path_hit_rate: active_metrics.path_hit_rate - metrics.path_hit_rate,
        delta_symbol_hit_rate: active_metrics.symbol_hit_rate - metrics.symbol_hit_rate,
        metrics,
    })
}

fn top_hits(hits: &[crate::search::lexical::SearchHit]) -> Vec<EvalSearchHit> {
    hits.iter()
        .enumerate()
        .map(|(index, hit)| EvalSearchHit {
            rank: index + 1,
            chunk_id: hit.chunk_id,
            path: hit.path.clone(),
            symbol_path: hit.symbol_path.clone(),
            start_line: hit.start_line,
            end_line: hit.end_line,
            score: hit.score,
        })
        .collect()
}

fn relevant(hit: &crate::search::lexical::SearchHit, query: &EvalQuery) -> bool {
    query.must_include_paths.iter().any(|path| path == &hit.path)
        || hit.symbol_path.as_deref().is_some_and(|symbol| {
            query
                .must_include_symbols
                .iter()
                .any(|expected| symbol == expected || symbol.ends_with(expected))
        })
        || query.must_include_graph_targets.iter().any(|expected| graph_hit_matches(hit, expected))
}

fn graph_hit_matches(hit: &crate::search::lexical::SearchHit, expected: &str) -> bool {
    let Some(graph) = &hit.graph else {
        return false;
    };
    graph.top_callers.iter().chain(graph.callers.iter()).any(|caller| {
        caller.symbol_path.ends_with(expected) || caller.symbol_path.contains(expected)
    }) || graph.top_callees.iter().chain(graph.callees.iter()).any(|callee| {
        callee.target == expected
            || callee.target.ends_with(expected)
            || callee
                .resolved_symbol_path
                .as_deref()
                .is_some_and(|symbol| symbol.ends_with(expected) || symbol.contains(expected))
    }) || graph.imports.iter().any(|import| import.target.contains(expected))
        || graph
            .referenced_types
            .iter()
            .any(|ty| ty.name == expected || ty.name.ends_with(expected))
}

fn missing(expected: &[String], found: &[String]) -> Vec<String> {
    let found = found.iter().collect::<BTreeSet<_>>();
    expected.iter().filter(|value| !found.contains(value)).cloned().collect()
}

fn find_current_source_violations(
    config: &Config,
    db: &IndexDatabase,
    hits: &[crate::search::lexical::SearchHit],
) -> anyhow::Result<Vec<CurrentSourceViolation>> {
    // `read_chunk_current` builds its own dict decoder per call; this eval pass is a cold CLI
    // diagnostic (not the hot retrieval path), so the per-call dict load is fine. It also drops the
    // graph + memory work the public `read_chunk` would do — the violation check only reads
    // path/text/line span (#77 Phase 2).
    let mut violations = Vec::new();
    let mut checked = BTreeSet::new();
    for hit in hits {
        if !checked.insert(hit.chunk_id) {
            continue;
        }
        match db.read_chunk_current(hit.chunk_id) {
            Ok(Some(chunk)) => {
                let source_path = config.root.join(&chunk.path);
                match fs::read_to_string(&source_path) {
                    Ok(source) => {
                        let current = slice_lines(&source, chunk.start_line, chunk.end_line);
                        if current.as_deref() != Some(chunk.text.as_str()) {
                            violations.push(CurrentSourceViolation {
                                chunk_id: hit.chunk_id,
                                path: chunk.path,
                                reason: "read_chunk text differs from current source line span"
                                    .to_string(),
                            });
                        }
                    },
                    Err(err) => violations.push(CurrentSourceViolation {
                        chunk_id: hit.chunk_id,
                        path: chunk.path,
                        reason: format!("current source unreadable: {err}"),
                    }),
                }
            },
            Ok(None) => violations.push(CurrentSourceViolation {
                chunk_id: hit.chunk_id,
                path: hit.path.clone(),
                reason: "search hit chunk is missing".to_string(),
            }),
            Err(err) => violations.push(CurrentSourceViolation {
                chunk_id: hit.chunk_id,
                path: hit.path.clone(),
                reason: format!("read_chunk failed: {err}"),
            }),
        }
    }
    Ok(violations)
}

fn slice_lines(source: &str, start_line: i64, end_line: i64) -> Option<String> {
    let start = usize::try_from(start_line).ok()?.max(1);
    let end = usize::try_from(end_line).ok()?.max(start);
    let lines = source.lines().collect::<Vec<_>>();
    if start > lines.len() {
        return None;
    }
    let mut text = lines[(start - 1)..end.min(lines.len())].join("\n");
    text.push('\n');
    Some(text)
}

fn normalize_kind(kind: &str) -> String {
    kind.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

fn aggregate(results: &[EvalQueryReport]) -> EvalMetrics {
    let measured = results.iter().filter(|result| !result.skipped).collect::<Vec<_>>();
    let query_count = measured.len().max(1) as f64;
    let total_hits = measured.iter().map(|r| r.top_hits.len() as u64).sum::<u64>();
    let stale = measured.iter().map(|r| r.stale_current_source_violations).sum::<u64>();
    let papertrail_samples =
        measured.iter().filter_map(|r| r.papertrail_precision_sample).collect::<Vec<_>>();
    EvalMetrics {
        mrr_at_10: measured.iter().map(|r| r.reciprocal_rank_at_10).sum::<f64>() / query_count,
        recall_at_10: measured.iter().map(|r| r.recall_at_10).sum::<f64>() / query_count,
        recall_at_3: measured.iter().map(|r| r.recall_at_3).sum::<f64>() / query_count,
        recall_at_returned: measured.iter().map(|r| r.recall_at_returned).sum::<f64>()
            / query_count,
        path_hit_rate: hit_rate(&measured, |r| r.missing_paths.is_empty()),
        symbol_hit_rate: hit_rate(&measured, |r| r.missing_symbols.is_empty()),
        graph_evidence_hit_rate: expected_hit_rate(&measured, |r| {
            (!r.graph_target_hits.is_empty() || !r.missing_graph_targets.is_empty())
                .then_some(r.missing_graph_targets.is_empty())
        }),
        impact_hit_rate: expected_hit_rate(&measured, |r| {
            (!r.impact_category_hits.is_empty()
                || !r.missing_impact_categories.is_empty()
                || !r.impact_path_hits.is_empty()
                || !r.missing_impact_paths.is_empty()
                || !r.impact_symbol_hits.is_empty()
                || !r.missing_impact_symbols.is_empty())
            .then_some(
                r.missing_impact_categories.is_empty()
                    && r.missing_impact_paths.is_empty()
                    && r.missing_impact_symbols.is_empty(),
            )
        }),
        git_evidence_hit_rate: expected_hit_rate(&measured, |r| {
            (!r.git_subject_hits.is_empty() || !r.missing_git_subjects.is_empty())
                .then_some(r.missing_git_subjects.is_empty())
        }),
        papertrail_evidence_hit_rate: expected_hit_rate(&measured, |r| {
            (!r.papertrail_kind_hits.is_empty() || !r.missing_papertrail_kinds.is_empty())
                .then_some(r.missing_papertrail_kinds.is_empty())
        }),
        stale_hit_rate: if total_hits == 0 { 0.0 } else { stale as f64 / total_hits as f64 },
        stale_current_source_violations: stale,
        current_source_violation_count: stale,
        papertrail_precision_sample: (!papertrail_samples.is_empty())
            .then(|| papertrail_samples.iter().sum::<f64>() / papertrail_samples.len() as f64),
        latency_p50_ms: percentile(measured.iter().map(|r| r.latency_ms).collect(), 0.50),
        latency_p95_ms: percentile(measured.iter().map(|r| r.latency_ms).collect(), 0.95),
    }
}

fn hit_rate(results: &[&EvalQueryReport], predicate: fn(&EvalQueryReport) -> bool) -> f64 {
    if results.is_empty() {
        return 1.0;
    }
    results.iter().filter(|result| predicate(result)).count() as f64 / results.len() as f64
}

fn expected_hit_rate(
    results: &[&EvalQueryReport],
    predicate: fn(&EvalQueryReport) -> Option<bool>,
) -> f64 {
    let applicable = results.iter().filter_map(|result| predicate(result)).collect::<Vec<_>>();
    if applicable.is_empty() {
        return 1.0;
    }
    applicable.iter().filter(|passed| **passed).count() as f64 / applicable.len() as f64
}

fn percentile(mut values: Vec<f64>, percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = ((values.len() - 1) as f64 * percentile).ceil() as usize;
    values[index.min(values.len() - 1)]
}

fn observed_expected(report: &EvalQueryReport) -> ExpectedQuery {
    let mut paths = report.top_hits.iter().map(|hit| hit.path.clone()).collect::<Vec<_>>();
    dedup(&mut paths);
    let mut symbols =
        report.top_hits.iter().filter_map(|hit| hit.symbol_path.clone()).collect::<Vec<_>>();
    dedup(&mut symbols);
    ExpectedQuery {
        id: report.id.clone(),
        must_include_paths: paths,
        must_include_symbols: symbols,
        must_include_graph_targets: report.graph_target_hits.clone(),
        must_include_impact_categories: report.impact_category_hits.clone(),
        must_include_impact_paths: report.impact_path_hits.clone(),
        must_include_impact_symbols: report.impact_symbol_hits.clone(),
        should_include_git_subjects: report.git_subject_hits.clone(),
        should_include_papertrail_kinds: report.papertrail_kind_hits.clone(),
    }
}

fn dedup(values: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn write_baseline(path: &Path, expected: Vec<ExpectedQuery>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(&BaselineSuite { expected })?;
    fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rag_rat_base::config::Config;

    use super::*;
    use crate::IndexDatabase;

    #[test]
    fn replay_eval_query_maps_commit_to_query() {
        use crate::index::git_history::ReplayCase;
        // Body present: query is subject + body; the diff's paths are the recall gold; id is the
        // 12-char short hash; symbols start empty (parent-state fills them in per case).
        let with_body = ReplayCase {
            hash: "0123456789abcdef0123".into(),
            subject: "fix(x): handle empty input".into(),
            body: "Closes #42".into(),
            changed_paths: vec!["src/a.rs".into(), "src/b.rs".into()],
        };
        let query = replay_eval_query(&with_body);
        assert_eq!(query.id, "replay-0123456789ab");
        assert_eq!(query.text, "fix(x): handle empty input\nCloses #42");
        assert_eq!(query.must_include_paths, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
        assert!(query.must_include_symbols.is_empty());

        // Empty/whitespace body: the query is the subject alone (no dangling newline).
        let no_body = ReplayCase { body: "  ".into(), ..with_body };
        assert_eq!(replay_eval_query(&no_body).text, "fix(x): handle empty input");
    }

    #[test]
    fn replay_gold_is_filtered_to_indexed_paths() {
        use crate::index::git_history::ReplayCase;
        let case = |paths: &[&str]| ReplayCase {
            hash: "0123456789abcdef0123".into(),
            subject: "fix something".into(),
            body: String::new(),
            changed_paths: paths.iter().map(|path| (*path).to_string()).collect(),
        };
        // The repo config indexes crates/** only; .github/**, tools/**, and root manifests are not.
        let indexed: BTreeSet<String> =
            ["crates/a.rs".to_string(), "crates/b.rs".to_string()].into_iter().collect();
        let cases = vec![
            case(&["crates/a.rs", ".github/workflows/ci.yml", "tools/x.sh"]), /* mixed -> keep
                                                                               * crates/a.rs */
            case(&[".github/only.yml", "Cargo.toml"]), // all non-indexed -> dropped
        ];
        let queries = replay_queries_with_indexed_gold(&cases, &indexed);
        assert_eq!(
            queries.len(),
            1,
            "the all-non-indexed case is dropped (no measurable gold left)"
        );
        assert_eq!(
            queries[0].must_include_paths,
            vec!["crates/a.rs".to_string()],
            "non-indexed gold (.github/**, tools/**, manifests) is filtered out of the denominator",
        );
    }

    #[test]
    fn eval_suite_reports_search_quality_and_current_source_safety() {
        let root = fixture_root();
        let mut config = Config::load(root.join("rag-rat.toml")).unwrap();
        // The fixture lives INSIDE this git repo, so the fixture's relative `database`
        // path resolves through shared_db_base to the enclosing repo's own self-index
        // (<repo>/.rag-rat/index.sqlite). Left alone, the test would both clobber the
        // live self-index and break: config.root (the fixture) would no longer match
        // the indexed content (rag-rat's real source). Redirect the DB to a unique temp
        // path while keeping config.root on the fixture so its source files resolve.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut db_dir = std::env::temp_dir();
        db_dir.push(format!(
            "rag-rat-eval-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&db_dir).unwrap();
        config.database = db_dir.join("index.sqlite");

        IndexDatabase::rebuild(&config).unwrap();

        // When an embedding backend is compiled in (default features), install its model and
        // reconcile so the eval exercises the real hybrid (lexical + semantic) retrieval path the
        // product ships. Under `--no-default-features` (hash embedder, no model) this block is
        // compiled out and the eval runs lexical-only — the baseline must pass either way. The
        // model download is cached in CI via `RAG_RAT_MODEL_CACHE` (see eval.yml).
        #[cfg(feature = "fastembed")]
        {
            let db = IndexDatabase::open_config(&config).unwrap();
            if let Some(model_id) = config.llm.embedding.backend.model_id() {
                db.install_model(model_id, config.llm.embedding.remote.as_ref())
                    .expect("install embedding model");
                db.reconcile(None, None).expect("reconcile embeddings");
            }
        }

        let report = run(&config, &EvalOptions {
            queries_path: workspace_root().join("evals/queries.toml"),
            expected_path: workspace_root().join("evals/expected_hits.toml"),
            update_baseline: false,
            scip_path: None,
            replay: None,
            rerank: false,
            search_limit: TOP_K,
        })
        .unwrap();
        // Safety + non-zero retrieval hold in every build shape.
        assert_eq!(report.metrics.stale_current_source_violations, 0);
        assert!(report.metrics.mrr_at_10 > 0.0);
        assert!(report.metrics.recall_at_10 > 0.0);

        // recall@3 measures membership over the first 3 hits, recall@10 over all 10, and
        // recall_at_returned over the WHOLE returned list; top-3 ⊆ top-10 ⊆ returned, so the chain
        // recall@3 <= recall@10 <= recall_at_returned can never invert — neither per query nor in
        // the aggregate mean. A regression here means a recall metric stopped slicing the same hit
        // list (or the limit leaked into a fixed cutoff's meaning).
        for result in report.results.iter().filter(|result| !result.skipped) {
            assert!(
                result.recall_at_3 <= result.recall_at_10,
                "{}: recall@3 ({}) exceeded recall@10 ({})",
                result.id,
                result.recall_at_3,
                result.recall_at_10,
            );
            assert!(
                result.recall_at_10 <= result.recall_at_returned,
                "{}: recall@10 ({}) exceeded recall_at_returned ({})",
                result.id,
                result.recall_at_10,
                result.recall_at_returned,
            );
        }
        assert!(report.metrics.recall_at_3 <= report.metrics.recall_at_10);
        assert!(report.metrics.recall_at_10 <= report.metrics.recall_at_returned);

        // The full hand-authored `must_include_*` baseline is the hard gate ONLY with an embedding
        // backend. Some expectations (the const-arrow hook by name, the graph-neighbor query)
        // require hybrid retrieval: their fixture chunks deliberately do NOT rank in top-K on
        // lexical/BM25 alone — we don't lexically seed the fixture to fake semantic recall (#163,
        // Codex review on #162). So under `--no-default-features` (hash, lexical-only) we
        // smoke-check above; the hybrid CI pass (default features → MiniLM) enforces the
        // full baseline here.
        #[cfg(feature = "fastembed")]
        {
            let failures = report
                .results
                .iter()
                .filter(|r| !r.passed)
                .map(|r| {
                    (r.id.as_str(), &r.missing_symbols, &r.missing_paths, &r.missing_graph_targets)
                })
                .collect::<Vec<_>>();
            assert!(report.pass, "eval baseline regressed: {failures:#?}");
        }

        // Best-effort cleanup; do not fail the test on cleanup error.
        let _ = std::fs::remove_dir_all(&db_dir);
    }

    /// When a `.scip` fixture is supplied, the eval suite runs the SCIP-oracle pass end-to-end
    /// through the public `IndexDatabase` API (`run_oracle` + `oracle_eval_metrics`, both scoped to
    /// the rebuilt fixture's active checkout) and attaches `OracleEvalMetrics` to the report. This
    /// exercises `run_oracle_eval` and the `query_api` oracle wrappers — the integration seam the
    /// unit tests can't reach (they call the `oracle::` functions directly on a synthetic conn).
    #[test]
    fn eval_suite_runs_oracle_when_scip_fixture_present() {
        use ::protobuf::Message;
        use ::scip::types::{
            Document, Index as ScipIndexProto, Occurrence, PositionEncoding, SymbolRole,
        };
        use protobuf::EnumOrUnknown;

        let root = fixture_root();
        let mut config = Config::load(root.join("rag-rat.toml")).unwrap();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut db_dir = std::env::temp_dir();
        db_dir.push(format!(
            "rag-rat-eval-oracle-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&db_dir).unwrap();
        config.database = db_dir.join("index.sqlite");

        IndexDatabase::rebuild(&config).unwrap();

        // A minimal but well-formed `.scip` over the fixture's `src/lib.rs`: a single reference to
        // `open_database` plus its definition. The pass reads `src/lib.rs`'s real bytes for the
        // position-encoding conversion, so the document path must match an indexed file.
        let symbol = "scip-rust crate held-mini `open_database`().";
        let index = ScipIndexProto {
            documents: vec![Document {
                relative_path: "src/lib.rs".to_string(),
                occurrences: vec![
                    // `open_database` is at line 2 (`pub fn open_database() {}`); its identifier
                    // sits after "pub fn " (7 bytes) → chars 7..20 on line 2.
                    Occurrence {
                        range: vec![2, 7, 20],
                        symbol: symbol.to_string(),
                        symbol_roles: SymbolRole::Definition as i32,
                        ..Default::default()
                    },
                ],
                position_encoding: EnumOrUnknown::new(
                    PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        let scip_path = db_dir.join("oracle.scip");
        std::fs::write(&scip_path, index.write_to_bytes().unwrap()).unwrap();

        let report = run(&config, &EvalOptions {
            queries_path: workspace_root().join("evals/queries.toml"),
            expected_path: workspace_root().join("evals/expected_hits.toml"),
            update_baseline: false,
            scip_path: Some(scip_path),
            replay: None,
            rerank: false,
            search_limit: TOP_K,
        })
        .unwrap();

        // The oracle ran and produced metrics (rates are all in [0, 1]); the exact values depend on
        // the fixture's edges, so we assert presence + bounds, not specific numbers.
        let oracle = report.oracle.expect("oracle metrics attached when a .scip is supplied");
        for rate in [
            oracle.precision,
            oracle.recall,
            oracle.name_only_recovery_rate,
            oracle.oracle_upgradeable_fraction,
        ] {
            assert!((0.0..=1.0).contains(&rate), "rate {rate} out of [0,1]");
        }

        // The run persisted an `oracle_runs` row + any verdicts; the status read (the other
        // `query_api` oracle wrapper) reflects them for the same tool/version.
        let db = IndexDatabase::open_config(&config).unwrap();
        let status = db.oracle_status(OracleTool::RustAnalyzer, EVAL_ORACLE_TOOL_VERSION).unwrap();
        assert_eq!(status.tool, "rust-analyzer");
        assert_eq!(status.tool_version, EVAL_ORACLE_TOOL_VERSION);
        assert_eq!(status.last_run_status.as_deref(), Some("Completed"));

        let _ = std::fs::remove_dir_all(&db_dir);
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().to_path_buf()
    }

    fn fixture_root() -> PathBuf {
        workspace_root().join("tests/fixtures/held-mini")
    }
}
