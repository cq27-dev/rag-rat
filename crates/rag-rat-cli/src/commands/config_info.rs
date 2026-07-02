//! Config / version introspection commands, split out of the `commands` god-module: `dump-config`
//! (the resolved config as JSON) and `version-check` (crates.io current-vs-latest).
use rag_rat_core::Config;

use crate::render::print_output;

pub(crate) fn dump_config(config: &Config) -> anyhow::Result<()> {
    let targets = config
        .targets
        .iter()
        .map(|target| {
            serde_json::json!({
                "name": target.name,
                "language": target.language.as_str(),
                "directories": target.directories,
                "include": target.include,
                "exclude": target.exclude,
                "kind": target.kind.as_str(),
            })
        })
        .collect::<Vec<_>>();
    print_output(&serde_json::json!({
        "root": config.root,
        "database": config.database,
        "llm": {
            "embedding": {
                "runtime": {
                    "batch_size": config.llm.embedding.runtime.batch_size,
                    "ort_threads": config.llm.embedding.runtime.ort_threads,
                    "omp_threads": config.llm.embedding.runtime.omp_threads,
                    "max_embedding_chars": config.llm.embedding.runtime.max_embedding_chars,
                }
            }
        },
        "targets": targets,
    }))
}

/// `version-check`: refresh the crates.io cache (network, synchronous — this is the explicit path)
/// and print current vs latest plus how to update. Best-effort: an offline/refused check still
/// prints the current version with a null latest. No network when disabled in config.
pub(crate) fn version_check(config: &Config) -> anyhow::Result<()> {
    use rag_rat_core::version_check;
    if !config.version_check.enabled {
        return print_output(&serde_json::json!({
            "enabled": false,
            "current_version": version_check::current_version(),
            "note": "version checking is disabled ([version_check] enabled = false in rag-rat.toml)",
        }));
    }
    // Prefer the just-fetched result; only fall back to the cache when the network fetch itself
    // failed — so a successful check still reports even if the cache write didn't land (read-only
    // checkout, full disk).
    let cached = version_check::refresh(&config.database)
        .or_else(|| version_check::read_cache(&config.database));
    print_output(&version_check::build_status(version_check::current_version(), cached.as_ref()))
}
