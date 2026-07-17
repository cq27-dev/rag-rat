//! Search / orientation commands, split out of the `commands` god-module: `query`, `brief`,
//! `clusters`, and `important_symbols` (with its auto-run ranking-hint nudge). All read-only over
//! an already-built index.
use rag_rat_base::config::Config;

use crate::cli::{BriefArgs, ClustersArgs, ImportantSymbolsArgs, QueryArgs};
use crate::open_index;
use crate::render::{print_output, print_query_explain};

pub(crate) fn query(config: &Config, args: &QueryArgs) -> anyhow::Result<()> {
    let query = args.query.join(" ");
    if query.trim().is_empty() {
        anyhow::bail!("query command needs a search string");
    }
    let db = open_index(config)?;
    if args.explain {
        print_query_explain(&db.search_explain(&query, 10, false)?);
        return Ok(());
    }
    print_output(&db.search(&query, 10, false)?)
}

pub(crate) fn brief(config: &Config, args: &BriefArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    let mode = rag_rat_query::repo_brief::RepoBriefMode::parse(args.mode.as_deref())?;
    print_output(&db.repo_brief(rag_rat_query::repo_brief::RepoBriefOptions {
        mode,
        limit: args.limit.unwrap_or(10),
        include_generated: args.include_generated,
        include_memories: !args.no_memories,
    })?)
}

pub(crate) fn clusters(config: &Config, args: &ClustersArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    print_output(&db.repo_clusters(rag_rat_core::query::clusters::RepoClustersOptions {
        limit: args.limit.unwrap_or(10),
        include_generated: args.include_generated,
        include_memories: !args.no_memories,
        min_cluster_size: args.min_cluster_size.unwrap_or(2),
    })?)
}

pub(crate) fn important_symbols(
    config: &Config,
    args: &ImportantSymbolsArgs,
) -> anyhow::Result<()> {
    let db = open_index(config)?;
    // CLI stays global-by-default: no auto-seed from the git diff (`auto_seed_from_diff: false`).
    // Only an explicit `--personalize` seeds — the intentional divergence from the MCP default.
    let mut result = db.important_symbols(rag_rat_core::index::ImportantSymbolsRequest {
        limit: args.limit.unwrap_or(20) as usize,
        personalize: args.personalize.clone(),
        auto_seed_from_diff: false,
    })?;
    apply_auto_run_ranking_hint(&mut result, config);
    print_output(&result)
}

/// Swap the heuristic-ranking nudge to the background-oracle wording when `[oracle] auto_run` is on
/// (the core method, config-unaware, emits the manual `oracle run` variant). No-op when the hint is
/// absent (a current oracle run exists) or auto_run is off.
pub(crate) fn apply_auto_run_ranking_hint(
    result: &mut rag_rat_query::pagerank::ImportantSymbolsResult,
    config: &Config,
) {
    if config.oracle.auto_run && result.ranking_hint.is_some() {
        result.ranking_hint = Some(rag_rat_query::pagerank::RANKING_HINT_AUTO_RUN.to_string());
    }
}
