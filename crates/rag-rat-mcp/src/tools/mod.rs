mod catalog;
mod defaults;
mod handlers;
mod requests;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

pub use catalog::*;
pub(crate) use defaults::*;
pub(crate) use handlers::*;
use rag_rat_core::language::Language;
use rag_rat_core::query::clusters::RepoClustersOptions;
use rag_rat_core::query::graph::{GraphResolutionMode, GraphTraversalOptions};
use rag_rat_core::query::graph_meta::GraphMetaMode;
use rag_rat_core::query::impact::ImpactSurfaceOptions;
use rag_rat_core::query::memory::{RepoMemoryBindTarget, RepoMemoryCreate, RepoMemoryUpdate};
use rag_rat_core::query::repo_brief::{RepoBriefMode, RepoBriefOptions};
use rag_rat_core::query::symbol::SymbolSelector;
use rag_rat_core::search::lexical::SearchOptions;
use rag_rat_core::{Config, IndexDatabase};
pub use requests::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Resolve one optional include-flag against the array form. `None` (the `include` key omitted)
/// keeps the tool's historical `default`; a present `include` list is the EXACT on-set, so a
/// default-on flag (e.g. impact's `tests`/`git`) is disabled simply by leaving it out. Presence in
/// the list = on. This is why the surface is an array, not per-key booleans (#149 follow-up).
fn included<T: PartialEq>(include: &Option<Vec<T>>, flag: T, default: bool) -> bool {
    match include {
        None => default,
        Some(flags) => flags.contains(&flag),
    }
}

pub fn call_tool(database: &Path, name: &str, arguments: Value) -> anyhow::Result<Value> {
    let db = IndexDatabase::open(database)?;
    call_tool_with_db(&db, name, arguments)
}

/// Tools that only read the index. They open a read-only connection so a concurrent writer can
/// never lock them out (#143). This is a writer DENY-list, not a reader allow-list, on purpose: a
/// newly added read tool is read-only by default (worst case: it falls back to a read-write open).
/// Every tool listed here mutates the index — keep it in sync with the write handlers in
/// `handlers.rs` (`tool_classification_covers_every_tool` guards that no tool is missed).
fn is_read_only_tool(name: &str) -> bool {
    !matches!(
        name,
        "heal_index"
            | "memory_create"
            | "memory_rebind"
            | "memory_update"
            | "memory_mark_obsolete"
            | "memory_validate"
    )
}

pub fn call_tool_for_config(
    config: &Config,
    name: &str,
    arguments: Value,
) -> anyhow::Result<Value> {
    // Read tools run on a lock-free read-only connection (#143). Two fall-backs to the read-write
    // open: (1) `try_open_config_read_only` returns `None` when the index still owes a heal/migrate
    // write; (2) a handful of read tools lazily WRITE on a cold path (`semantic_search` heals stale
    // FTS, `read_chunk` heals a stale/deleted file, `git_blame_chunk` fills the blame cache), which
    // fails on the read-only connection with `SQLITE_READONLY` — we detect that and retry the whole
    // call read-write (which performs the heal). The warm path never writes, so it stays lock-free.
    if is_read_only_tool(name)
        && let Some(db) = IndexDatabase::try_open_config_read_only(config)?
    {
        match call_tool_with_db(&db, name, arguments.clone()) {
            Ok(result) => return finalize_tool_result(config, name, result),
            Err(err) if rag_rat_core::storage::is_readonly_violation(&err) => {
                // A lazy write hit the read-only connection — fall through to the read-write open.
            },
            Err(err) => return Err(err),
        }
    }
    let db = IndexDatabase::open_config(config)?;
    let result = call_tool_with_db(&db, name, arguments)?;
    finalize_tool_result(config, name, result)
}

/// Post-process a tool result before returning it. Currently only `index_status`: surface the
/// crates.io version status so an agent can see (and relay) when a newer rag-rat is published.
/// Read-only — it reads the cached check (refreshed out of band), never the network, so the tool
/// stays fast; omitted entirely when version checking is disabled.
fn finalize_tool_result(config: &Config, name: &str, mut result: Value) -> anyhow::Result<Value> {
    if name == "index_status"
        && let Some(version) = rag_rat_core::version_check::cached_status(
            config.version_check.enabled,
            &config.database,
        )
        && let Some(obj) = result.as_object_mut()
    {
        obj.insert("version".to_string(), serde_json::json!(version));
    }
    // Mirror the CLI's `apply_auto_run_ranking_hint`: when the background auto-fresh oracle is on,
    // rewrite `important_symbols`' heuristic nudge so it doesn't tell the agent to run `oracle run`
    // by hand — compiler ranking refreshes on its own. The core query is config-unaware, so the
    // rewrite happens here where the config is available. (#142 review)
    if name == "important_symbols"
        && config.oracle.auto_run
        && let Some(obj) = result.as_object_mut()
        && obj.get("ranking_hint").and_then(Value::as_str).is_some()
    {
        obj.insert(
            "ranking_hint".to_string(),
            serde_json::json!(rag_rat_core::query::pagerank::RANKING_HINT_AUTO_RUN),
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
