//! Config / version introspection commands, split out of the `commands` god-module: `dump-config`
//! (the resolved config as JSON) and `version-check` (crates.io current-vs-latest).
use rag_rat_base::config::Config;

use crate::render::print_output;

pub(crate) fn dump_config(config: &Config) -> anyhow::Result<()> {
    let targets = config
        .targets
        .iter()
        .map(|target| {
            serde_json::json!({
                "name": target.name,
                "language": target.language.as_db_str(),
                "directories": target.directories,
                "include": target.include,
                "exclude": target.exclude,
                "kind": target.kind.as_db_str(),
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

/// Background thread that keeps the crates.io version cache fresh for the long-lived MCP server:
/// refresh-if-stale now, then poll on a sub-TTL cadence so a release that lands while the server
/// stays up is picked up within `DEFAULT_TTL_MS` (not only at restart) — `index_status` and the
/// SessionStart digest read that cache. Best-effort + non-blocking: the actual crates.io call runs
/// at most once per TTL (gated by `needs_refresh`), fail-open; the thread dies with the process (a
/// hot-upgrade re-exec re-spawns it). No-op when version checking is disabled.
pub(crate) fn spawn_detached_version_refresh(config: &rag_rat_base::config::Config) {
    use rag_rat_core::version_check;
    /// Poll cadence — well under the TTL so the once-per-day network refresh actually fires on a
    /// server that outlives the TTL, while the cache read in between is trivially cheap.
    const POLL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
    if !config.version_check.enabled {
        return;
    }
    let database = config.database.clone();
    std::thread::spawn(move || {
        loop {
            let now = rag_rat_base::time::now_ms();
            if version_check::needs_refresh(
                version_check::read_cache(&database).as_ref(),
                now,
                version_check::DEFAULT_TTL_MS,
            ) {
                let _ = version_check::refresh(&database);
            }
            std::thread::sleep(POLL);
        }
    });
}
