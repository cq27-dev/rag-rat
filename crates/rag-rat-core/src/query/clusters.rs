use std::collections::{BTreeMap, BTreeSet, HashMap};

use rag_rat_query::repo_brief::{
    self, FileBriefRow, RepoBriefMemoryCounts, enrich_memory_counts, enrich_symbol_kinds,
};
use rusqlite::Connection;
use serde::Serialize;

// The per-commit co-touch cap is shared with the persisted change-coupling pass
// (`impact_surface`) so the transient (this file) and windowed/persisted (change_coupling)
// consumers can't drift.
use crate::index::change_coupling::MAX_COUPLING_COMMIT_FILES as MAX_COTOUCH_COMMIT_FILES;

const MAX_PATH_BUCKET_FILES: usize = 120;
const MAX_CLUSTER_PATHS: usize = 8;
const MIN_EDGE_SCORE: f64 = 0.34;

#[derive(Debug, Clone, Copy)]
pub struct RepoClustersOptions {
    pub limit: u32,
    pub include_generated: bool,
    pub include_memories: bool,
    pub min_cluster_size: u32,
}

impl Default for RepoClustersOptions {
    fn default() -> Self {
        Self { limit: 10, include_generated: false, include_memories: true, min_cluster_size: 2 }
    }
}

#[derive(Debug, Serialize)]
pub struct RepoClustersReport {
    pub summary: RepoClustersSummary,
    pub clusters: Vec<RepoCluster>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RepoClustersSummary {
    pub total_files: u64,
    pub clustered_files: u64,
    pub singleton_files: u64,
    pub returned_clusters: u64,
    pub generated_files: u64,
    pub git_commits: u64,
    pub git_file_changes: u64,
    pub graph_edges: u64,
    pub repo_memories: RepoBriefMemoryCounts,
    pub scoring_note: String,
}

#[derive(Debug, Serialize)]
pub struct RepoCluster {
    pub cluster_id: String,
    pub name: String,
    pub category: String,
    pub confidence: String,
    pub score: f64,
    pub metrics: RepoClusterMetrics,
    pub representative_paths: Vec<String>,
    pub why: Vec<String>,
    pub next_tools: Vec<RepoClusterNextTool>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct RepoClusterMetrics {
    pub file_count: u64,
    pub total_lines: u64,
    pub total_chunks: u64,
    pub total_symbols: u64,
    pub fan_in: u64,
    pub fan_out: u64,
    pub commit_touch_count: u64,
    pub recent_touch_count: u64,
    pub additions: u64,
    pub deletions: u64,
    pub co_touch_edges: u64,
    pub graph_edges: u64,
    pub languages: BTreeMap<String, u64>,
    pub kinds: BTreeMap<String, u64>,
    pub memories: RepoBriefMemoryCounts,
}

#[derive(Debug, Serialize)]
pub struct RepoClusterNextTool {
    pub tool: String,
    pub reason: String,
    pub args: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct PathPair(usize, usize);

impl PathPair {
    fn new(left: usize, right: usize) -> Self {
        if left <= right { Self(left, right) } else { Self(right, left) }
    }
}

#[derive(Debug, Default, Clone)]
struct PairEvidence {
    co_touch_count: u64,
    graph_edge_count: u64,
    path_candidate: bool,
}

#[derive(Debug, Clone)]
struct ScoredPair {
    pair: PathPair,
    score: f64,
}

#[derive(Debug, Clone)]
struct ClusterBuild {
    rows: Vec<FileBriefRow>,
    avg_edge_score: f64,
    co_touch_edges: u64,
    graph_edges: u64,
}

pub fn repo_clusters(
    conn: &Connection,
    options: RepoClustersOptions,
) -> anyhow::Result<RepoClustersReport> {
    let summary = repo_brief::summary_counts(conn)?;
    let mut warnings = Vec::new();
    if summary.git_commits == 0 {
        warnings.push("git history is not indexed; co-touch clustering is unavailable".to_string());
    }
    if summary.graph_edges == 0 {
        warnings.push(
            "graph edges are not indexed; graph-neighborhood clustering is unavailable".to_string(),
        );
    }

    let mut rows = repo_brief::file_rows(conn, options.include_generated)?;
    if options.include_memories {
        enrich_memory_counts(conn, &mut rows)?;
    }
    enrich_symbol_kinds(conn, &mut rows)?;

    let co_touch_pairs = co_touch_pairs(conn, options.include_generated, &rows)?;
    let graph_pairs = graph_pairs(conn, options.include_generated, &rows)?;
    let clusters = cluster_rows(&rows, &co_touch_pairs, &graph_pairs, options.min_cluster_size);
    let clustered_files = clusters.iter().map(|cluster| cluster.rows.len() as u64).sum::<u64>();
    let mut clusters = clusters.into_iter().map(cluster_for).collect::<Vec<_>>();
    clusters.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.metrics.file_count.cmp(&a.metrics.file_count))
            .then_with(|| a.name.cmp(&b.name))
    });
    for (index, cluster) in clusters.iter_mut().enumerate() {
        cluster.cluster_id = format!("cluster_{:03}", index + 1);
    }

    let limit = usize::try_from(options.limit.max(1)).unwrap_or(usize::MAX);
    clusters.truncate(limit);
    let singleton_files = rows.len() as u64 - clustered_files;

    Ok(RepoClustersReport {
        summary: RepoClustersSummary {
            total_files: summary.total_files,
            clustered_files,
            singleton_files,
            returned_clusters: clusters.len() as u64,
            generated_files: summary.generated_files,
            git_commits: summary.git_commits,
            git_file_changes: summary.git_file_changes,
            graph_edges: summary.graph_edges,
            repo_memories: summary.memories,
            scoring_note: "cheap sparse ownership clustering: path proximity, direct graph edges, \
                           bounded git co-touches, churn, and metadata"
                .to_string(),
        },
        clusters,
        warnings,
    })
}

fn cluster_rows(
    rows: &[FileBriefRow],
    co_touch_pairs: &BTreeMap<PathPair, u64>,
    graph_pairs: &BTreeMap<PathPair, u64>,
    min_cluster_size: u32,
) -> Vec<ClusterBuild> {
    if rows.is_empty() {
        return Vec::new();
    }

    let candidates = candidate_pairs(rows, co_touch_pairs, graph_pairs);
    let mut parent = UnionFind::new(rows.len());
    let mut accepted_pairs = Vec::new();
    for pair in candidates {
        if pair.score >= MIN_EDGE_SCORE {
            parent.union(pair.pair.0, pair.pair.1);
            accepted_pairs.push(pair);
        }
    }

    let mut groups = BTreeMap::<usize, Vec<usize>>::new();
    for index in 0..rows.len() {
        groups.entry(parent.find(index)).or_default().push(index);
    }

    let min_cluster_size = usize::try_from(min_cluster_size.max(2)).unwrap_or(usize::MAX);
    let mut clusters = Vec::new();
    for indexes in groups.values().filter(|indexes| indexes.len() >= min_cluster_size) {
        let index_set = indexes.iter().copied().collect::<BTreeSet<_>>();
        let mut edge_score_sum = 0.0;
        let mut edge_count = 0_u64;
        let mut co_touch_edges = 0_u64;
        let mut graph_edges = 0_u64;
        for pair in &accepted_pairs {
            if index_set.contains(&pair.pair.0) && index_set.contains(&pair.pair.1) {
                edge_score_sum += pair.score;
                edge_count += 1;
                co_touch_edges += co_touch_pairs.get(&pair.pair).copied().unwrap_or(0);
                graph_edges += graph_pairs.get(&pair.pair).copied().unwrap_or(0);
            }
        }
        let avg_edge_score = if edge_count == 0 { 0.0 } else { edge_score_sum / edge_count as f64 };
        clusters.push(ClusterBuild {
            rows: indexes.iter().map(|index| rows[*index].clone()).collect(),
            avg_edge_score,
            co_touch_edges,
            graph_edges,
        });
    }
    clusters
}

fn candidate_pairs(
    rows: &[FileBriefRow],
    co_touch_pairs: &BTreeMap<PathPair, u64>,
    graph_pairs: &BTreeMap<PathPair, u64>,
) -> Vec<ScoredPair> {
    let mut evidence = BTreeMap::<PathPair, PairEvidence>::new();
    for (pair, count) in co_touch_pairs {
        evidence.entry(*pair).or_default().co_touch_count = *count;
    }
    for (pair, count) in graph_pairs {
        evidence.entry(*pair).or_default().graph_edge_count = *count;
    }
    for pair in path_candidate_pairs(rows) {
        evidence.entry(pair).or_default().path_candidate = true;
    }

    evidence
        .into_iter()
        .filter_map(|(pair, evidence)| {
            let score = pair_score(&rows[pair.0], &rows[pair.1], &evidence);
            (score > 0.0).then_some(ScoredPair { pair, score })
        })
        .collect()
}

fn pair_score(left: &FileBriefRow, right: &FileBriefRow, evidence: &PairEvidence) -> f64 {
    let path = path_similarity(&left.path, &right.path);
    let same_bucket = path_bucket(&left.path) == path_bucket(&right.path);
    if !same_bucket {
        return 0.0;
    }
    let co_touch = capped(evidence.co_touch_count as f64 / 4.0);
    let graph = capped(evidence.graph_edge_count as f64 / 6.0);
    let same_language = if left.language == right.language { 1.0 } else { 0.0 };
    let same_kind = if left.kind == right.kind { 1.0 } else { 0.0 };
    let churn = churn_similarity(left, right);
    let path_weight = if evidence.path_candidate || same_bucket { 0.43 } else { 0.25 };
    capped(
        path * path_weight
            + co_touch * 0.24
            + graph * 0.22
            + same_language * 0.06
            + same_kind * 0.03
            + churn * 0.02,
    )
}

fn co_touch_pairs(
    conn: &Connection,
    include_generated: bool,
    rows: &[FileBriefRow],
) -> anyhow::Result<BTreeMap<PathPair, u64>> {
    let path_index = path_index(rows);
    // `git_file_changes` is direct-scoped (V040); the `files` view join is by PATH, so the
    // explicit `repo_id` predicate is what keeps a sibling repo's co-touch history for a shared
    // path out of this repo's clusters in a consolidated DB.
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT git_file_changes.commit_hash, git_file_changes.path
        FROM git_file_changes
        JOIN files ON files.path = git_file_changes.path
        WHERE (?1 OR files.generated = 0) AND git_file_changes.repo_id = ?2
        ORDER BY git_file_changes.commit_hash, git_file_changes.path
        ",
    )?;
    let mut query_rows = stmt.query((include_generated, &repo_id))?;
    let mut pairs = BTreeMap::<PathPair, u64>::new();
    let mut current_commit = String::new();
    let mut paths = Vec::new();
    while let Some(row) = query_rows.next()? {
        let commit = row.get::<_, String>(0)?;
        let path = row.get::<_, String>(1)?;
        if current_commit.is_empty() {
            current_commit = commit.clone();
        }
        if commit != current_commit {
            add_co_touch_pairs(&path_index, &paths, &mut pairs);
            paths.clear();
            current_commit = commit;
        }
        paths.push(path);
    }
    add_co_touch_pairs(&path_index, &paths, &mut pairs);
    Ok(pairs)
}

fn add_co_touch_pairs(
    path_index: &HashMap<&str, usize>,
    paths: &[String],
    pairs: &mut BTreeMap<PathPair, u64>,
) {
    if paths.len() < 2 || paths.len() > MAX_COTOUCH_COMMIT_FILES {
        return;
    }
    let mut indexes =
        paths.iter().filter_map(|path| path_index.get(path.as_str()).copied()).collect::<Vec<_>>();
    indexes.sort_unstable();
    indexes.dedup();
    for left in 0..indexes.len() {
        for right in (left + 1)..indexes.len() {
            *pairs.entry(PathPair::new(indexes[left], indexes[right])).or_default() += 1;
        }
    }
}

fn graph_pairs(
    conn: &Connection,
    include_generated: bool,
    rows: &[FileBriefRow],
) -> anyhow::Result<BTreeMap<PathPair, u64>> {
    let path_index = path_index(rows);
    let bucket_by_path = rows
        .iter()
        .map(|row| (row.path.as_str(), path_bucket(&row.path)))
        .collect::<HashMap<_, _>>();
    let mut stmt = conn.prepare(
        "
        SELECT source_files.path, target_files.path
        FROM edges
        JOIN files source_files ON source_files.id = edges.source_file_id
        JOIN symbols target_symbols ON target_symbols.id = edges.to_symbol_id
        JOIN files target_files ON target_files.id = target_symbols.file_id
        WHERE source_files.path != target_files.path
          AND (?1 OR (source_files.generated = 0 AND target_files.generated = 0))
        ",
    )?;
    let mut pairs = BTreeMap::<PathPair, u64>::new();
    let rows = stmt.query_map([include_generated], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (left, right) = row?;
        let (Some(left_bucket), Some(right_bucket)) =
            (bucket_by_path.get(left.as_str()), bucket_by_path.get(right.as_str()))
        else {
            continue;
        };
        if left_bucket != right_bucket {
            continue;
        }
        let (Some(left), Some(right)) =
            (path_index.get(left.as_str()), path_index.get(right.as_str()))
        else {
            continue;
        };
        *pairs.entry(PathPair::new(*left, *right)).or_default() += 1;
    }
    Ok(pairs)
}

fn path_candidate_pairs(rows: &[FileBriefRow]) -> Vec<PathPair> {
    let buckets = indexed_path_buckets(rows);
    let mut pairs = Vec::new();
    for indexes in buckets.values() {
        if indexes.len() < 2 || indexes.len() > MAX_PATH_BUCKET_FILES {
            continue;
        }
        for left in 0..indexes.len() {
            for right in (left + 1)..indexes.len() {
                pairs.push(PathPair::new(indexes[left], indexes[right]));
            }
        }
    }
    pairs
}

fn cluster_for(cluster: ClusterBuild) -> RepoCluster {
    let mut metrics = RepoClusterMetrics::default();
    let mut rows = cluster.rows;
    rows.sort_by(|a, b| {
        file_representative_score(b)
            .total_cmp(&file_representative_score(a))
            .then_with(|| a.path.cmp(&b.path))
    });

    for row in &rows {
        metrics.file_count += 1;
        metrics.total_lines += row.line_count;
        metrics.total_chunks += row.chunk_count;
        metrics.total_symbols += row.symbol_count;
        metrics.fan_in += row.fan_in;
        metrics.fan_out += row.fan_out;
        metrics.commit_touch_count += row.commit_touch_count;
        metrics.recent_touch_count += row.recent_touch_count;
        metrics.additions += row.additions;
        metrics.deletions += row.deletions;
        *metrics.languages.entry(row.language.clone()).or_default() += 1;
        *metrics.kinds.entry(row.kind.clone()).or_default() += 1;
        metrics.memories.active += row.memories.active;
        metrics.memories.stale += row.memories.stale;
        metrics.memories.obsolete += row.memories.obsolete;
        metrics.memories.other += row.memories.other;
    }
    metrics.co_touch_edges = cluster.co_touch_edges;
    metrics.graph_edges = cluster.graph_edges;

    let representative_paths =
        rows.iter().take(MAX_CLUSTER_PATHS).map(|row| row.path.clone()).collect::<Vec<_>>();
    let name = cluster_name(&rows);
    let score = cluster_score(&metrics, cluster.avg_edge_score);
    let category = cluster_category(&metrics).to_string();
    let confidence = confidence_for(&metrics, cluster.avg_edge_score).to_string();
    let why = why_for(&metrics, cluster.avg_edge_score, &name);
    let next_tools = next_tools_for(representative_paths.first().map(String::as_str));

    RepoCluster {
        cluster_id: String::new(),
        name,
        category,
        confidence,
        score,
        metrics,
        representative_paths,
        why,
        next_tools,
    }
}

fn file_representative_score(row: &FileBriefRow) -> f64 {
    capped((row.fan_in + row.fan_out) as f64 / 50.0) * 0.40
        + capped(row.symbol_count as f64 / 40.0) * 0.25
        + capped(row.commit_touch_count as f64 / 25.0) * 0.25
        + capped(row.line_count as f64 / 800.0) * 0.10
}

fn cluster_score(metrics: &RepoClusterMetrics, avg_edge_score: f64) -> f64 {
    rag_rat_query::round_score(capped(
        avg_edge_score * 0.35
            + capped(metrics.file_count as f64 / 12.0) * 0.12
            + capped((metrics.fan_in + metrics.fan_out) as f64 / 100.0) * 0.18
            + capped(metrics.commit_touch_count as f64 / 60.0) * 0.15
            + capped(metrics.total_symbols as f64 / 120.0) * 0.12
            + capped((metrics.co_touch_edges + metrics.graph_edges) as f64 / 20.0) * 0.08,
    ))
}

fn cluster_category(metrics: &RepoClusterMetrics) -> &'static str {
    if metrics.co_touch_edges > 0 && metrics.graph_edges > 0 {
        "graph_and_churn_ownership"
    } else if metrics.co_touch_edges > 0 {
        "co_touch_ownership"
    } else if metrics.graph_edges > 0 {
        "graph_neighborhood"
    } else {
        "path_proximity"
    }
}

fn confidence_for(metrics: &RepoClusterMetrics, avg_edge_score: f64) -> &'static str {
    if (metrics.co_touch_edges > 0 && metrics.graph_edges > 0) || avg_edge_score >= 0.55 {
        "high"
    } else if metrics.co_touch_edges > 0 || metrics.graph_edges > 0 || avg_edge_score >= 0.42 {
        "medium"
    } else {
        "low"
    }
}

fn why_for(metrics: &RepoClusterMetrics, avg_edge_score: f64, name: &str) -> Vec<String> {
    let mut why = vec![format!("shared ownership bucket: {name}")];
    if metrics.graph_edges > 0 {
        why.push(format!(
            "graph neighborhood: {} direct indexed cross-file edges",
            metrics.graph_edges
        ));
    }
    if metrics.co_touch_edges > 0 {
        why.push(format!(
            "co-touch history: {} bounded same-commit file-pair hits",
            metrics.co_touch_edges
        ));
    }
    if metrics.commit_touch_count > 0 {
        why.push(format!(
            "churn: {} total commit touches, {} recent",
            metrics.commit_touch_count, metrics.recent_touch_count
        ));
    }
    if metrics.total_symbols > 0 {
        why.push(format!(
            "source shape: {} files, {} symbols, {} lines",
            metrics.file_count, metrics.total_symbols, metrics.total_lines
        ));
    }
    why.push(format!("average accepted edge score: {:.3}", avg_edge_score));
    why
}

fn next_tools_for(path: Option<&str>) -> Vec<RepoClusterNextTool> {
    let Some(path) = path else {
        return Vec::new();
    };
    vec![
        next_tool(
            "repo_brief",
            "compare cluster representatives against spine/churn/god-module rankings",
            [("mode", "refactor_candidates".to_string())],
        ),
        next_tool(
            "impact_surface",
            "inspect graph, git, papertrail, and memories for the representative path",
            [("query", path.to_string())],
        ),
        next_tool("git_history_for_path", "inspect co-touch and churn history", [(
            "path",
            path.to_string(),
        )]),
    ]
}

fn next_tool<const N: usize>(
    tool: &str,
    reason: &str,
    args: [(&str, String); N],
) -> RepoClusterNextTool {
    RepoClusterNextTool {
        tool: tool.to_string(),
        reason: reason.to_string(),
        args: args
            .into_iter()
            .map(|(key, value)| (key.to_string(), serde_json::Value::String(value)))
            .collect(),
    }
}

fn cluster_name(rows: &[FileBriefRow]) -> String {
    let paths = rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>();
    let common = common_parent(&paths);
    if !common.is_empty() {
        return common;
    }
    let dominant_language = dominant_key(rows.iter().map(|row| row.language.as_str()));
    format!("{dominant_language} files")
}

fn common_parent(paths: &[&str]) -> String {
    let mut iter = paths.iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut common = parent_components(first);
    for path in iter {
        let components = parent_components(path);
        let keep =
            common.iter().zip(components.iter()).take_while(|(left, right)| left == right).count();
        common.truncate(keep);
    }
    common.join("/")
}

fn dominant_key<'a>(values: impl Iterator<Item = &'a str>) -> String {
    let mut counts = BTreeMap::<&str, u64>::new();
    for value in values {
        *counts.entry(value).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|(left_key, left_count), (right_key, right_count)| {
            left_count.cmp(right_count).then_with(|| right_key.cmp(left_key))
        })
        .map(|(value, _)| value.to_string())
        .unwrap_or_else(|| "source".to_string())
}

fn path_similarity(left: &str, right: &str) -> f64 {
    let left_components = path_components(left);
    let right_components = path_components(right);
    let common_prefix = left_components
        .iter()
        .zip(right_components.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let prefix = if left_components.is_empty() && right_components.is_empty() {
        0.0
    } else {
        common_prefix as f64 / left_components.len().max(right_components.len()) as f64
    };
    let left_tokens = path_tokens(left);
    let right_tokens = path_tokens(right);
    let union = left_tokens.union(&right_tokens).count();
    let token_overlap = if union == 0 {
        0.0
    } else {
        left_tokens.intersection(&right_tokens).count() as f64 / union as f64
    };
    prefix * 0.65 + token_overlap * 0.35
}

fn churn_similarity(left: &FileBriefRow, right: &FileBriefRow) -> f64 {
    let left_churn = left.additions.saturating_add(left.deletions);
    let right_churn = right.additions.saturating_add(right.deletions);
    let max_churn = left_churn.max(right_churn);
    let churn = if max_churn == 0 {
        0.0
    } else {
        1.0 - (left_churn.abs_diff(right_churn) as f64 / max_churn as f64)
    };
    let max_touches = left.commit_touch_count.max(right.commit_touch_count);
    let touches = if max_touches == 0 {
        0.0
    } else {
        1.0 - (left.commit_touch_count.abs_diff(right.commit_touch_count) as f64
            / max_touches as f64)
    };
    (churn + touches) * 0.5
}

fn path_bucket(path: &str) -> String {
    let mut components = parent_components(path);
    components.truncate(5);
    match components.as_slice() {
        [] => ".".to_string(),
        [one] => one.clone(),
        _ => components.join("/"),
    }
}

fn indexed_path_buckets(rows: &[FileBriefRow]) -> BTreeMap<String, Vec<usize>> {
    let mut buckets = BTreeMap::<String, Vec<usize>>::new();
    for (index, row) in rows.iter().enumerate() {
        buckets.entry(path_bucket(&row.path)).or_default().push(index);
    }
    buckets
}

fn parent_components(path: &str) -> Vec<String> {
    let mut components = path_components(path);
    components.pop();
    components
}

fn path_components(path: &str) -> Vec<String> {
    path.split('/').filter(|part| !part.is_empty()).map(str::to_string).collect()
}

fn path_tokens(path: &str) -> BTreeSet<String> {
    path.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| part.len() > 1)
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

fn path_index(rows: &[FileBriefRow]) -> HashMap<&str, usize> {
    rows.iter().enumerate().map(|(index, row)| (row.path.as_str(), index)).collect()
}

fn capped(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

#[derive(Debug)]
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(len: usize) -> Self {
        Self { parent: (0..len).collect(), rank: vec![0; len] }
    }

    fn find(&mut self, value: usize) -> usize {
        if self.parent[value] != value {
            self.parent[value] = self.find(self.parent[value]);
        }
        self.parent[value]
    }

    fn union(&mut self, left: usize, right: usize) {
        let left_root = self.find(left);
        let right_root = self.find(right);
        if left_root == right_root {
            return;
        }
        if self.rank[left_root] < self.rank[right_root] {
            self.parent[left_root] = right_root;
        } else if self.rank[left_root] > self.rank[right_root] {
            self.parent[right_root] = left_root;
        } else {
            self.parent[right_root] = left_root;
            self.rank[left_root] += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_similarity_prefers_nearby_files() {
        let nearby = path_similarity("src/actors/sync/actor.rs", "src/actors/sync/msg.rs");
        let distant = path_similarity("src/actors/sync/actor.rs", "apps/mobile/src/App.tsx");
        assert!(nearby > distant, "nearby={nearby} distant={distant}");
    }

    #[test]
    fn cluster_rows_uses_cotouch_edges() {
        let rows =
            vec![row("src/sync/actor.rs"), row("src/sync/msg.rs"), row("apps/mobile/App.tsx")];
        let co_touch_pairs = BTreeMap::from([(PathPair::new(0, 1), 3)]);
        let graph_pairs = BTreeMap::new();

        let clusters = cluster_rows(&rows, &co_touch_pairs, &graph_pairs, 2);

        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].rows.len(), 2);
        assert_eq!(clusters[0].co_touch_edges, 3);
    }

    fn row(path: &str) -> FileBriefRow {
        FileBriefRow {
            path: path.to_string(),
            language: "rust".to_string(),
            kind: "source".to_string(),
            generated: false,
            line_count: 10,
            chunk_count: 1,
            symbol_count: 1,
            fan_in: 0,
            fan_out: 0,
            commit_touch_count: 1,
            recent_touch_count: 0,
            additions: 10,
            deletions: 0,
            papertrail_ref_count: 0,
            symbol_kinds: BTreeMap::new(),
            memories: RepoBriefMemoryCounts::default(),
        }
    }
}
