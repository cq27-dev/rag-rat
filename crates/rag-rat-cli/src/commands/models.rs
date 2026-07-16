//! Embedding-model + eval/benchmark commands, split out of the `commands` god-module: `models`
//! (list / install, with the remote-block guard + short-context warning) and — behind the `eval`
//! feature — `eval` and `benchmark-embedding` with their spec helpers.
#[cfg(feature = "eval")]
use std::path::PathBuf;

use rag_rat_base::config::Config;
#[cfg(feature = "eval")]
use rag_rat_core::OutputFormat;

#[cfg(feature = "eval")]
use crate::cli::{BenchmarkEmbeddingArgs, EvalArgs};
use crate::cli::{ModelsArgs, ModelsCommand};
#[cfg(feature = "eval")]
use crate::commands::output_format;
use crate::open_index;
#[cfg(feature = "eval")]
use crate::render::print_eval_summary;
use crate::render::print_output;

#[cfg(feature = "eval")]
pub(crate) fn eval(config: &Config, args: &EvalArgs) -> anyhow::Result<()> {
    // Parent-state replay is a distinct flow: it scores each case against its own freshly-built
    // parent index (no shared HEAD db, no static suite / oracle baseline), so it has its own entry
    // point and report shape rather than threading through `run`.
    if args.replay_parent_state {
        let report = rag_rat_core::eval::run_replay_parent_state(
            config,
            &rag_rat_core::eval::ReplayOptions {
                max_cases: args.replay_max_cases,
                max_files: args.replay_max_files,
            },
        )?;
        print_output(&report)?;
        return Ok(());
    }
    let options = rag_rat_core::eval::EvalOptions {
        queries_path: args
            .queries
            .clone()
            .unwrap_or_else(|| default_eval_path(config, "queries.toml")),
        expected_path: args
            .expected
            .clone()
            .unwrap_or_else(|| default_eval_path(config, "expected_hits.toml")),
        update_baseline: args.update_baseline,
        scip_path: args.scip.clone().or_else(|| {
            let default = default_eval_path(config, "oracle.scip");
            default.exists().then_some(default)
        }),
        replay: args.replay.then_some(rag_rat_core::eval::ReplayOptions {
            max_cases: args.replay_max_cases,
            max_files: args.replay_max_files,
        }),
        rerank: args.rerank,
        search_limit: args.search_limit,
    };
    let report = rag_rat_core::eval::run(config, &options)?;
    // `eval` prints a greppable human summary by default; the global `--json` (or a baseline
    // rewrite, which needs the machine record) switches to the structured report.
    if output_format() == OutputFormat::Json || options.update_baseline {
        print_output(&report)?;
    } else {
        print_eval_summary(&report);
    }
    if !report.pass {
        anyhow::bail!(
            "eval failed: stale_current_source_violations={}, failed_queries={}",
            report.metrics.stale_current_source_violations,
            report.results.iter().filter(|result| !result.passed).count()
        );
    }
    Ok(())
}

#[cfg(feature = "eval")]
pub(crate) fn default_eval_path(config: &Config, file_name: &str) -> PathBuf {
    config.root.join("evals").join(file_name)
}

/// `benchmark-embedding` (#346): provision an ephemeral cookbook box, sweep embedding throughput
/// across concurrency candidates, and emit per-candidate texts/s as JSON — then tear the box down.
/// The point is a machine-readable comparison of backends (ollama/infinity/vLLM) and concurrency
/// levels, so the PRIMARY output is JSON regardless of the global `--json` render flag.
///
/// This runs its OWN measured sweep (`benchmark_remote_concurrency`), NOT the caching auto-tuner:
/// every candidate is measured and reported (no knee selection, no tune-cache write). The box is
/// provisioned with `tune = None` and kept bound for the whole sweep — `ProvisionedBox::drop` is
/// the teardown, so letting it live to the end of scope is what tears it down cleanly.
#[cfg(feature = "eval")]
pub(crate) fn benchmark_embedding(
    config: &Config,
    args: &BenchmarkEmbeddingArgs,
) -> anyhow::Result<()> {
    use rag_rat_base::config::RemoteEmbeddingConfig;

    // Build an EPHEMERAL remote config directly (no rag-rat.toml round-trip): `cookbook` set,
    // `endpoint` None. Struct construction bypasses the config layer's connect/ephemeral
    // validation, which is fine — the benchmark only provisions + sweeps, it never reconciles.
    //
    // Base it on the repo's configured `[remote]` block (or defaults) so the benchmark mirrors the
    // LIVE reconcile REQUEST SHAPE — `batch_size` / `max_batch_chars` / `request_timeout_s` /
    // `num_ctx` are carried over via `..base`; only the provisioning + CLI-selected fields are
    // overridden. Filling the request shape from `default()` instead would benchmark a different
    // request than this repo's reconcile actually sends.
    let base = config.llm.embedding.remote.clone().unwrap_or_default();
    let cap = base.bounded_concurrency();

    let max_embedding_chars = config.llm.embedding.runtime.max_embedding_chars;
    // The candidate ladder: explicit `--candidates` when given, else the tuner's default ladder for
    // the config's concurrency cap (powers of two up to the cap, plus the exact cap).
    let candidates: Vec<u32> = if args.candidates.is_empty() {
        rag_rat_llm::throughput_tune::default_benchmark_candidates(cap)
    } else {
        // Normalize explicit `--candidates`: clamp each to the effective range (the server +
        // embedder cap concurrency at 1..=MAX, so a raw 1024 would measure the 512 cap
        // while labeled 1024), then sort + dedupe — the sweep assumes ASCENDING candidates
        // (it stops on the first over-allocation window). Without this, `--candidates
        // 1024,1` could stop before the valid `1` row or mislabel a row after starting a
        // paid box.
        let mut c: Vec<u32> = args
            .candidates
            .iter()
            .map(|&c| RemoteEmbeddingConfig::bounded_concurrency_value(c))
            .collect();
        c.sort_unstable();
        c.dedup();
        c
    };

    // Size the provisioned server for the HIGHEST fan-out the sweep will test: `concurrency` is
    // forwarded as `server_concurrency` (ollama `OLLAMA_NUM_PARALLEL` / vLLM `--max-num-seqs`;
    // infinity ignores it). Explicit `--candidates` above the cap would otherwise drive client
    // fan-outs the server was NOT launched to handle, so those rows would look slow / fail for the
    // wrong reason. Take the max candidate (never below the cap), clamped to the global ceiling.
    let provision_concurrency = RemoteEmbeddingConfig::bounded_concurrency_value(
        candidates.iter().copied().max().unwrap_or(cap).max(cap),
    );
    let remote = RemoteEmbeddingConfig {
        model: args.model.clone(),
        backend: args.backend,
        endpoint: None,
        cookbook: Some(args.cookbook.clone()),
        query_endpoint: None,
        auth_env: None,
        gpu: args.gpu.clone(),
        concurrency: provision_concurrency,
        ..base
    };
    let budget_ms =
        args.budget_ms.unwrap_or_else(rag_rat_llm::throughput_tune::default_benchmark_budget_ms);

    // Reject a budget too small to measure ANY candidate BEFORE provisioning a paid box: the sweep
    // floors each candidate at a ~1s slice and stops once <1s of the budget remains, so a tiny
    // `--budget-ms` would provision + tear down a box while measuring zero rows.
    let min_budget = rag_rat_llm::throughput_tune::min_benchmark_budget_ms(candidates.len());
    anyhow::ensure!(
        budget_ms >= min_budget,
        "--budget-ms {budget_ms} is too small to benchmark {} candidate(s): need at least \
         {min_budget} ms (~1s per candidate). Raise --budget-ms or pass fewer --candidates.",
        candidates.len(),
    );

    // Registry model → trust `spec.dim`. Off-registry HF model → provision, measure the dim from
    // one probe embed, then benchmark. Either way the ProvisionedBox is kept bound for the
    // whole sweep.
    let spec = rag_rat_base::embedding_models::spec(&args.model);
    let provisioned = rag_rat_llm::cookbook_internals::provision_box_for_benchmark(
        &remote,
        spec_or_measure_placeholder(spec),
    )?;
    let (selected_model_id, dim) = match spec {
        Some(spec) => (spec.model_id.to_string(), spec.dim),
        None => {
            // Off-registry: learn the dim from the server's first response.
            let dim = rag_rat_llm::throughput_tune::measure_remote_dim(
                &provisioned.endpoint,
                provisioned.auth_token.as_deref(),
                &remote,
            )?;
            (args.model.clone(), dim)
        },
    };

    let measured = rag_rat_llm::throughput_tune::benchmark_remote_concurrency(
        &provisioned.endpoint,
        provisioned.auth_token.as_deref(),
        &remote,
        &selected_model_id,
        dim,
        max_embedding_chars,
        &candidates,
        budget_ms,
    );

    // Surface any REQUESTED candidates the sweep did NOT measure. `measure_candidates` drops a
    // candidate (and every higher one) when its probe window exceeds `MAX_PROBE_WINDOW_BYTES` or
    // the budget runs out, rather than caching a partial sweep — fine for the auto-tuner, but
    // the benchmark would otherwise exit successfully with rows silently missing after starting
    // a paid box. Report them (and warn on stderr) so the JSON is honest about coverage.
    let measured_set: std::collections::BTreeSet<u32> =
        measured.iter().map(|m| m.concurrency).collect();
    let skipped: Vec<u32> =
        candidates.iter().copied().filter(|c| !measured_set.contains(c)).collect();
    if !skipped.is_empty() {
        eprintln!(
            "benchmark-embedding: WARNING — {} requested candidate(s) not measured (probe window \
             / budget limit): {skipped:?}. Lower --candidates or [runtime] max_embedding_chars, \
             or raise --budget-ms.",
            skipped.len(),
        );
    }

    // Peak = the highest-throughput row among rows that actually measured something AND stayed
    // stable (`requests > 0 && !aborted`). A failed row (`requests == 0`) or a breaker-tripped
    // overloaded row (`aborted`) must not be advertised as the best result — `peak` is the
    // machine-readable backend/concurrency selector, so an all-failed or all-unstable run reports
    // `peak: null` rather than a misleading number.
    let peak = measured
        .iter()
        .filter(|m| m.requests > 0 && !m.aborted)
        .max_by(|a, b| a.texts_per_second.total_cmp(&b.texts_per_second))
        .map(|m| serde_json::json!({ "concurrency": m.concurrency, "texts_per_second": m.texts_per_second }));

    let report = serde_json::json!({
        "backend": args.backend.as_db_str(),
        "model": args.model,
        "cookbook": args.cookbook,
        "gpu": args.gpu,
        "dim": dim,
        "budget_ms": budget_ms,
        "candidates": measured,
        "skipped_candidates": skipped,
        "peak": peak,
    });

    // JSON is the PRIMARY output (#346), regardless of the global render flag. To a file when
    // `--output` is set, else stdout.
    let json = serde_json::to_string_pretty(&report)?;
    match &args.output {
        Some(path) => {
            crate::write_atomic(path, json.as_bytes())?;
            eprintln!(
                "benchmark-embedding: wrote {} candidate rows to {}",
                measured.len(),
                path.display()
            );
        },
        None => println!("{json}"),
    }
    // `provisioned` drops here → the box is torn down (SIGTERM → grace → SIGKILL on its group).
    Ok(())
}

/// The `spec` param `provision_box_for_benchmark` needs is `&EmbeddingModelSpec`; an off-registry
/// model has none, so provisioning uses the FALLBACK all-MiniLM spec purely to satisfy the type —
/// it only feeds `spec.model_id`/`spec.dim` into the built-but-discarded probe embedder inside
/// `provision_and_build`, which the benchmark never uses (it constructs its own per-candidate
/// embedders against the box). The real server-side model is `remote.model`; the real dim is
/// measured separately via `measure_remote_dim`.
#[cfg(feature = "eval")]
fn spec_or_measure_placeholder(
    spec: Option<&'static rag_rat_base::embedding_models::EmbeddingModelSpec>,
) -> &'static rag_rat_base::embedding_models::EmbeddingModelSpec {
    spec.unwrap_or_else(|| {
        rag_rat_base::embedding_models::spec(rag_rat_base::embedding_models::FASTEMBED_MODEL_ID)
            .expect("the fallback all-MiniLM spec is always registered")
    })
}

/// Decide whether a `models install <model_id>` should use the configured `[llm.embedding.remote]`
/// block. The block is configured for ONE specific model — the SELECTED `[llm.embedding] model` —
/// and serves `[remote] model` (e.g. MiniLM) over Ollama. Reusing it for a DIFFERENT transformer id
/// (e.g. `BAAI/bge-small-en-v1.5`, also FastEmbed/384) would pass the non-transformer guard + the
/// 384-dim probe yet mark the BGE row `runtime='ollama'` while the server actually embeds MiniLM
/// under the BGE id (#330). So:
/// - no `[remote]` block → `None` (local install for whatever the user typed);
/// - `[remote]` + the user installs the CONFIGURED model → the remote (serve it over Ollama);
/// - `[remote]` + a DIFFERENT model → a clear error (don't silently install the wrong model).
fn remote_for_install<'a>(
    config: &'a Config,
    model_id: &str,
) -> anyhow::Result<Option<&'a rag_rat_base::config::RemoteEmbeddingConfig>> {
    let Some(remote) = config.llm.embedding.remote.as_ref() else {
        return Ok(None);
    };
    // Resolve the requested id to its canonical spec id and compare to the configured selected
    // model.
    let requested = rag_rat_base::embedding_models::spec(model_id).map(|s| s.model_id);
    let configured = config.llm.embedding.backend.model_id();
    if requested.is_some() && requested == configured {
        Ok(Some(remote))
    } else {
        anyhow::bail!(
            "remote embedding is configured for `{}`; install that model remotely, or remove the \
             [llm.embedding.remote] block to install `{model_id}` locally",
            configured.unwrap_or("none"),
        )
    }
}

pub(crate) fn models(config: &Config, args: &ModelsArgs) -> anyhow::Result<()> {
    let db = open_index(config)?;
    match &args.command {
        None | Some(ModelsCommand::List) => print_output(&db.list_models()?),
        Some(ModelsCommand::Install { model_id }) => {
            warn_if_short_context(model_id);
            let remote = remote_for_install(config, model_id)?;
            print_output(&db.install_model(model_id, remote)?)
        },
    }
}

/// One-line heads-up when installing a SHORT-CONTEXT embedding model — one whose token window is
/// smaller than the default chunk-embed budget, so typical code chunks get truncated (their tail is
/// not embedded), costing precision/recall on large functions. `rag-rat init`'s model help covers
/// this interactively; this catches the `rag-rat models install` CLI path.
fn warn_if_short_context(model_id: &str) {
    let Some(spec) = rag_rat_base::embedding_models::spec(model_id) else { return };
    let (Some(max_tokens), Some(model_chars)) = (spec.max_tokens, spec.max_input_chars()) else {
        return;
    };
    if model_chars < rag_rat_core::index::ai::DEFAULT_MAX_EMBEDDING_CHARS {
        eprintln!(
            "note: {model_id} has a {max_tokens}-token context, so code chunks longer than that \
             are truncated — their tail is not embedded, costing precision/recall on large \
             functions. For code, a long-context model like jinaai/jina-embeddings-v2-base-code \
             (8192 tokens) embeds whole chunks."
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rag_rat_base::config::Config;

    static N: AtomicU64 = AtomicU64::new(0);

    /// Build a `Config` from a written rag-rat.toml with the given embedding-model selector and an
    /// optional connect `[remote]` block (a closed-port endpoint — never connected in these tests).
    fn config_with_remote(model: &str, with_remote: bool) -> (PathBuf, Config) {
        let root = std::env::temp_dir().join(format!(
            "rag-rat-cli-remote-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn a() {}\n").unwrap();
        let remote = if with_remote {
            "\n[llm.embedding.remote]\nendpoint = \"http://127.0.0.1:1\"\nmodel = \"all-minilm\"\n"
        } else {
            ""
        };
        std::fs::write(
            root.join("rag-rat.toml"),
            format!(
                "[index]\nroot = \".\"\n\n[target_bindings]\nrust = \
                 [\"src\"]\n\n[llm.embedding]\nmodel = \"{model}\"\n{remote}"
            ),
        )
        .unwrap();
        let config = Config::load(root.join("rag-rat.toml")).unwrap();
        (root, config)
    }

    #[test]
    fn remote_for_install_only_applies_the_remote_block_to_the_configured_model() {
        // Configured for the MiniLM transformer over a [remote] block.
        let (root, config) = config_with_remote("sentence-transformers/all-MiniLM-L6-v2", true);

        // Installing the CONFIGURED model → uses the remote block.
        assert!(
            super::remote_for_install(&config, "sentence-transformers/all-MiniLM-L6-v2")
                .unwrap()
                .is_some(),
            "the configured model installs over the remote",
        );

        // Installing a DIFFERENT transformer (BGE, also FastEmbed/384) → REJECTED (#330): the
        // remote serves MiniLM, so installing BGE over it would store MiniLM vectors under
        // the BGE id.
        let err = super::remote_for_install(&config, "BAAI/bge-small-en-v1.5")
            .expect_err("a different model than the configured one must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("remote embedding is configured for"), "{msg}");
        assert!(msg.contains("sentence-transformers/all-MiniLM-L6-v2"), "names configured: {msg}");
        assert!(msg.contains("BAAI/bge-small-en-v1.5"), "names requested: {msg}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_for_install_returns_none_without_a_remote_block() {
        // No [remote] block → any install is local (None) regardless of the requested id.
        let (root, config) = config_with_remote("sentence-transformers/all-MiniLM-L6-v2", false);
        assert!(super::remote_for_install(&config, "BAAI/bge-small-en-v1.5").unwrap().is_none());
        assert!(super::remote_for_install(&config, "embedding-hash").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
