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
use rag_rat_core::config::MemorySurface;
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
    // The bare-path entry point has no `Config`, so config-gated knobs default OFF here: the
    // graded-git rerank (`[search] graded_git_rerank`) and the memory surface (`[memory] surface`)
    // are only read on the `call_tool_for_config` path.
    call_tool_with_db(&db, name, arguments, false, MemorySurface::default())
}

/// Tools that only read the index. They open a read-only connection so a concurrent writer can
/// never lock them out (#143). This is a writer DENY-list, not a reader allow-list, on purpose: a
/// newly added read tool is read-only by default (worst case: it falls back to a read-write open).
/// Every tool listed here mutates the index — keep it in sync with the write handlers in
/// `handlers.rs` (`tool_classification_covers_every_tool` guards that no tool is missed).
pub(crate) fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        "heal_index"
            | "memory_create"
            | "memory_rebind"
            | "memory_update"
            | "memory_mark_obsolete"
            | "memory_validate"
    )
}

fn is_read_only_tool(name: &str) -> bool {
    !is_write_tool(name)
}

/// Read tools that compare indexed graph/symbol data against LIVE on-disk source text, read through
/// the stored `source_root` (the MAIN checkout). Under a linked-worktree overlay scope these tools'
/// GRAPH side would come from the branch overlay while their TEXT side still read the main
/// checkout, so matched / text-only / graph-only would be computed against mismatched sides. They
/// are kept in the BASE scope even when a `worktree` is passed — the diagnostic stays
/// self-consistent (graph and text both from main) rather than producing wrong overlay-vs-main
/// results. Accepted recall: the diagnostic doesn't reflect branch-only changes when invoked with
/// `worktree` (#219 review).
fn tool_compares_against_live_source(name: &str) -> bool {
    matches!(name, "compare_graph_to_text")
}

pub fn call_tool_for_config(
    config: &Config,
    name: &str,
    arguments: Value,
) -> anyhow::Result<Value> {
    // Optional caller `worktree`: scope the query to a linked worktree's branch overlay instead of
    // the indexed checkout (#219). Extracted as a COMMON field from the request (serde ignores it
    // per-tool — no `deny_unknown_fields`), so every tool routes without per-arg-struct plumbing.
    // `use_worktree_scope` validates it server-side and falls back to the base scope for an absent
    // / main / foreign / unreadable path — and only writes the per-connection `temp.*` scope
    // view, so it is safe even on the read-only connection.
    let worktree = worktree_arg(&arguments);
    // A tool that compares the graph against LIVE main-checkout text stays BASE-scoped even with a
    // `worktree` arg, so its graph and text sides come from the same checkout (#219 review).
    let scope_worktree = if tool_compares_against_live_source(name) { None } else { worktree };
    // Read tools run on a lock-free read-only connection (#143). Two fall-backs to the read-write
    // open: (1) `try_open_config_read_only` returns `None` when the index still owes a heal/migrate
    // write; (2) a handful of read tools lazily WRITE on a cold path (`semantic_search` heals stale
    // FTS, `read_chunk` heals a stale/deleted file, `git_blame_chunk` fills the blame cache), which
    // fails on the read-only connection with `SQLITE_READONLY` — we detect that and retry the whole
    // call read-write (which performs the heal). The warm path never writes, so it stays lock-free.
    if is_read_only_tool(name)
        && let Some(mut db) = IndexDatabase::try_open_config_read_only(config)?
    {
        db.use_worktree_scope(&config.root, scope_worktree.as_deref())?;
        match call_tool_with_db(
            &db,
            name,
            arguments.clone(),
            config.search.graded_git_rerank,
            config.memory.surface,
        ) {
            Ok(result) => return finalize_tool_result(config, name, result),
            Err(err) if rag_rat_core::storage::is_readonly_violation(&err) => {
                // A lazy write hit the read-only connection — fall through to the read-write open.
            },
            Err(err) => return Err(err),
        }
    }
    // The read-WRITE open, reached by (a) a genuine write tool, or (b) a read tool whose lazy write
    // tripped `SQLITE_READONLY` (or whose RO open was unavailable). A WRITE tool stays in the BASE
    // scope: `heal_index` / the `memory_*` tools read file bytes from the stored `source_root` (the
    // MAIN checkout) but would write into whatever scope the connection carries, so scoping the
    // connection to a linked worktree would reindex the overlay with MAIN's contents or tombstone
    // branch-only files — corrupting the overlay that only `index_worktree_overlay` may maintain. A
    // READ tool keeps the worktree scope so its query still serves the overlay on this fallback;
    // its lazy heal can't corrupt the overlay because the heal paths skip writes under a linked
    // overlay scope (`IndexDatabase::active_scope_is_linked_overlay`) (#219 review).
    // A concurrent writer (the watcher mid-pass, an `index` pass, the post-commit `maintenance`
    // hook) can hold the write lock past this connection's `busy_timeout`, so `open_config` + the
    // call can still fail `SQLITE_BUSY` ("database is locked"). Retry the whole read-write attempt
    // with bounded backoff rather than surfacing a `-32603` to the agent (#220). The common path —
    // no writer, or the read-only open above — never reaches this fallback.
    let result = with_busy_retry(|| {
        let mut db = IndexDatabase::open_config(config)?;
        if is_read_only_tool(name) {
            db.use_worktree_scope(&config.root, scope_worktree.as_deref())?;
        }
        call_tool_with_db(
            &db,
            name,
            arguments.clone(),
            config.search.graded_git_rerank,
            config.memory.surface,
        )
    })?;
    finalize_tool_result(config, name, result)
}

/// Run a read-write tool attempt, retrying on `SQLITE_BUSY`/`SQLITE_LOCKED` with bounded
/// exponential backoff. A writer can hold the lock past the connection's `busy_timeout`; rather
/// than surface that to the agent as a `-32603`, re-open and retry a few times (#220). Bounded so a
/// sustained writer (a rare full rebuild during interactive use) eventually returns the error
/// instead of blocking indefinitely. Non-busy errors return immediately.
fn with_busy_retry<T>(mut attempt: impl FnMut() -> anyhow::Result<T>) -> anyhow::Result<T> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut tries = 0u32;
    loop {
        match attempt() {
            Err(err) if tries + 1 < MAX_ATTEMPTS && rag_rat_core::storage::is_busy(&err) => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(25 * (1 << tries)));
            },
            result => return result,
        }
    }
}

/// The caller `worktree` (a linked-worktree checkout path): the explicit `worktree` request field,
/// else the MCP server's own working directory. `resolve_worktree_scope` downstream VALIDATES it
/// (non-worktree / foreign-repo / unreadable → base scope), so the cwd fallback is safe
/// best-effort: an agent working IN a linked worktree gets that branch's overlay without passing
/// the param, while a server rooted at and launched in the main checkout still resolves to base.
/// The explicit param stays the reliable path (the server's cwd is launch-dependent — see #219).
/// Common to every tool, read here rather than declared on each arg struct.
fn worktree_arg(arguments: &Value) -> Option<std::path::PathBuf> {
    worktree_arg_or_cwd(arguments, std::env::current_dir().ok())
}

/// Testable core of [`worktree_arg`]: explicit request field (trimmed; blank → ignored), else
/// `cwd`.
fn worktree_arg_or_cwd(
    arguments: &Value,
    cwd: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    arguments
        .get("worktree")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or(cwd)
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
