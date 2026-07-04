//! `consolidate`: import this repo's legacy per-repo index into the consolidated global database
//! (memory-sync phase A7). The logic lives in `rag_rat_core::index::consolidate`; this is the thin
//! CLI shim that renders the outcome (a pinned `database` key is refused inside `run` with the
//! remove-the-key remedy, so every rendered import is a completed import + rename).
use rag_rat_core::Config;
use rag_rat_core::index::consolidate::{self, ConsolidateOutcome};

use crate::render::print_output;

pub(crate) fn consolidate(config: &Config) -> anyhow::Result<()> {
    let outcome = consolidate::run(config)?;
    match &outcome {
        ConsolidateOutcome::AlreadyGlobal { database } => print_output(&serde_json::json!({
            "status": "already_global",
            "database": database,
            "note": "this repo already uses the consolidated global database",
        })),
        ConsolidateOutcome::AlreadyImported { imported } => print_output(&serde_json::json!({
            "status": "already_consolidated",
            "imported": imported,
            "note": "already imported (an `.imported` marker is present); nothing to do",
        })),
        ConsolidateOutcome::NoLegacyIndex { source } => print_output(&serde_json::json!({
            "status": "no_legacy_index",
            "source": source,
            "note": "no legacy per-repo index found to import",
        })),
        ConsolidateOutcome::Imported(summary) => print_output(&serde_json::json!({
            "status": "imported",
            "repo_id": summary.repo_id,
            "target": summary.target,
            "imported_from": summary.source,
            "renamed_to": summary.renamed_to,
            "memories": summary.memories,
            "bindings": summary.bindings,
            "tags": summary.tags,
            "call_paths": summary.call_paths,
            "call_path_edges": summary.call_path_edges,
            "embedding_cache_rows": summary.embedding_cache_rows,
            "meta_keys": summary.meta_keys,
            "next": "run `rag-rat index --full` to rebuild the derived index — the carried \
                     embedding cache makes re-embedding a no-op",
        })),
    }
}
