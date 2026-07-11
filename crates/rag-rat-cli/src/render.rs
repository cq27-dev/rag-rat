use super::*;

#[cfg(feature = "eval")]
pub(crate) fn print_eval_summary(report: &rag_rat_core::eval::EvalReport) {
    println!(
        "eval: pass={} queries={} skipped={} mrr@10={:.3} recall@10={:.3} recall@3={:.3} \
         recall@returned={:.3} path_hit_rate={:.3} symbol_hit_rate={:.3}",
        report.pass,
        report.queries,
        report.results.iter().filter(|result| result.skipped).count(),
        report.metrics.mrr_at_10,
        report.metrics.recall_at_10,
        report.metrics.recall_at_3,
        report.metrics.recall_at_returned,
        report.metrics.path_hit_rate,
        report.metrics.symbol_hit_rate
    );
    println!(
        "eval: stale_current_source_violations={} stale_hit_rate={:.3} latency_p50_ms={:.1} \
         latency_p95_ms={:.1}",
        report.metrics.stale_current_source_violations,
        report.metrics.stale_hit_rate,
        report.metrics.latency_p50_ms,
        report.metrics.latency_p95_ms
    );
    println!(
        "eval: graph_evidence_hit_rate={:.3} impact_hit_rate={:.3} git_evidence_hit_rate={:.3} \
         papertrail_evidence_hit_rate={:.3}",
        report.metrics.graph_evidence_hit_rate,
        report.metrics.impact_hit_rate,
        report.metrics.git_evidence_hit_rate,
        report.metrics.papertrail_evidence_hit_rate
    );
    if let Some(precision) = report.metrics.papertrail_precision_sample {
        println!("eval: papertrail_precision_sample={precision:.3}");
    }
    if let Some(oracle) = &report.oracle {
        println!("{}", format_oracle_line(oracle));
    }
    if report.oracle_skipped_drifted > 0 {
        println!(
            "eval: WARNING oracle skipped {} candidate(s) on drifted source (file_sha mismatch); \
             rebuild the index against the .scip checkout for complete oracle metrics",
            report.oracle_skipped_drifted,
        );
    }
    println!(
        "eval: hash_vector_baseline model={} available={} current_artifacts={} mrr@10={:.3} \
         recall@10={:.3} recall@3={:.3} recall@returned={:.3} delta_mrr@10={:+.3} \
         delta_recall@10={:+.3} delta_recall@3={:+.3} delta_recall@returned={:+.3}",
        report.hash_vector_baseline.model_id,
        report.hash_vector_baseline.available,
        report.hash_vector_baseline.current_artifacts,
        report.hash_vector_baseline.metrics.mrr_at_10,
        report.hash_vector_baseline.metrics.recall_at_10,
        report.hash_vector_baseline.metrics.recall_at_3,
        report.hash_vector_baseline.metrics.recall_at_returned,
        report.hash_vector_baseline.delta_mrr_at_10,
        report.hash_vector_baseline.delta_recall_at_10,
        report.hash_vector_baseline.delta_recall_at_3,
        report.hash_vector_baseline.delta_recall_at_returned
    );
    for result in report.results.iter().filter(|result| !result.passed) {
        println!(
            "eval: failed {} missing_paths={:?} missing_symbols={:?} missing_graph_targets={:?} \
             missing_impact_categories={:?} missing_impact_paths={:?} missing_impact_symbols={:?} \
             missing_git_subjects={:?} missing_papertrail_kinds={:?} \
             stale_current_source_violations={}",
            result.id,
            result.missing_paths,
            result.missing_symbols,
            result.missing_graph_targets,
            result.missing_impact_categories,
            result.missing_impact_paths,
            result.missing_impact_symbols,
            result.missing_git_subjects,
            result.missing_papertrail_kinds,
            result.stale_current_source_violations
        );
    }
    for result in report.results.iter().filter(|result| result.skipped) {
        println!(
            "eval: skipped {} reason={}",
            result.id,
            result.skip_reason.as_deref().unwrap_or("not applicable")
        );
    }
}
pub(crate) fn print_query_explain(hits: &[SearchHit]) {
    for (index, hit) in hits.iter().enumerate() {
        if index > 0 {
            println!();
        }
        println!(
            "{}:{}-{} {}",
            hit.path,
            hit.start_line,
            hit.end_line,
            hit.symbol_path.as_deref().unwrap_or("<chunk>")
        );
        println!("score: {:.3}", hit.score);
        if let Some(components) = &hit.score_components {
            println!("  bm25: {:.3}", components.bm25);
            println!("  vector: {:.3}", components.vector);
            println!("  symbol: {:.3}", components.symbol);
            println!("  graph: {:.3}", components.graph);
            println!("  git: {:.3}", components.git);
            println!("  github: {:.3}", components.github);
            if let Some(note) = &components.vector_note {
                println!("  vector_note: {note}");
            }
        }
        println!("summary:");
        for line in hit.summary.lines() {
            println!("  {line}");
        }
    }
}
pub(crate) fn print_reconcile_plan(plan: &rag_rat_core::index::ai::ReconcilePlan) {
    let embeddings = &plan.embeddings;
    println!("Embeddings");
    println!("  model: {}", embeddings.model_id);
    println!("  model_version: {}", embeddings.model_version);
    println!("  dim: {}", embeddings.dim);
    println!("  available: {}", embeddings.available);
    if let Some(message) = &embeddings.message {
        println!("  message: {message}");
    }
    println!("  current: {}", embeddings.current);
    println!("  missing: {}", embeddings.missing);
    println!("  stale: {}", embeddings.stale);
    println!("  model_changed: {}", embeddings.model_changed);
    println!("  dim_changed: {}", embeddings.dim_changed);
    println!("  failed_retryable: {}", embeddings.failed_retryable);
    println!("  failed_waiting: {}", embeddings.failed_waiting);
    println!("  blocked: {}", embeddings.blocked);
    println!("  skipped: {}", embeddings.skipped_total);
    if !embeddings.skipped_by_policy.is_empty() {
        println!("  skipped_by_policy:");
        for (policy, count) in &embeddings.skipped_by_policy {
            println!("    {policy}: {count}");
        }
    }
    if !embeddings.missing_by_priority.is_empty() {
        println!("  missing_by_priority:");
        for (priority, count) in &embeddings.missing_by_priority {
            println!("    {priority}: {count}");
        }
    }
    println!();
    println!("Summaries");
    println!(
        "  {}",
        if plan.summaries.enabled { "enabled" } else { plan.summaries.message.as_str() }
    );
}
pub(crate) fn render_reconcile_progress(progress: rag_rat_core::index::ai::ReconcileProgress) {
    match progress {
        rag_rat_core::index::ai::ReconcileProgress::Started {
            model_id,
            total_chunks,
            batch_size,
        } => {
            eprintln!("reconcile: model={model_id} chunks={total_chunks} batch_size={batch_size}");
        },
        rag_rat_core::index::ai::ReconcileProgress::Batch {
            processed_chunks,
            total_chunks,
            embeddings_written,
            failed_chunks,
            blocked_chunks,
        } => {
            let percent = progress_percent(processed_chunks, total_chunks);
            eprintln!(
                "reconcile: {processed_chunks}/{total_chunks} ({percent:>3}%) \
                 written={embeddings_written} failed={failed_chunks} blocked={blocked_chunks}"
            );
        },
        rag_rat_core::index::ai::ReconcileProgress::Finished {
            processed_chunks,
            embeddings_written,
            failed_chunks,
            blocked_chunks,
        } => {
            eprintln!(
                "reconcile: complete processed={processed_chunks} written={embeddings_written} \
                 failed={failed_chunks} blocked={blocked_chunks}"
            );
        },
    }
}
pub(crate) fn progress_percent(current: u64, total: u64) -> u64 {
    current.saturating_mul(100).checked_div(total).unwrap_or(100).min(100)
}
pub(crate) fn render_papertrail_sync_progress(
    progress: rag_rat_core::index::papertrail::PapertrailSyncProgress,
) {
    match progress.action {
        PapertrailSyncAction::Syncing => eprintln!(
            "github sync: {}/{} fetching {}/{}#{}",
            progress.current, progress.total, progress.owner, progress.repo, progress.number
        ),
        PapertrailSyncAction::Skipped => eprintln!(
            "github sync: {}/{} skip cached {}/{}#{}",
            progress.current, progress.total, progress.owner, progress.repo, progress.number
        ),
        PapertrailSyncAction::Synced => eprintln!(
            "github sync: {}/{} synced {}/{}#{}",
            progress.current, progress.total, progress.owner, progress.repo, progress.number
        ),
        PapertrailSyncAction::Failed => eprintln!(
            "github sync: {}/{} failed {}/{}#{}: {}",
            progress.current,
            progress.total,
            progress.owner,
            progress.repo,
            progress.number,
            progress.message.unwrap_or_else(|| "unknown error".to_string())
        ),
        PapertrailSyncAction::RebuildingFts => {
            eprintln!("github sync: rebuilding GitHub FTS cache")
        },
    }
}
/// Print a serializable result in the process-wide output format (TOON by default, JSON under
/// `--json`). Renamed from `print_json` because it is no longer always JSON — the format is the
/// global flag's choice, read from `output_format()`.
pub(crate) fn print_output(value: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", rag_rat_core::render(value, output_format()));
    Ok(())
}
pub(crate) fn render_index_progress(progress: IndexProgress) {
    match progress {
        IndexProgress::Started { database, mode } => {
            eprintln!("index: {} using {}", mode.label(), database.display());
        },
        IndexProgress::Discovering => {
            eprintln!("index: discovering files");
        },
        IndexProgress::Discovered { files } => {
            eprintln!("index: discovered {files} files");
        },
        IndexProgress::PreparingFile { current, total, path, language, kind } => {
            let percent = progress_percent(
                u64::try_from(current).unwrap_or(u64::MAX),
                u64::try_from(total).unwrap_or(u64::MAX),
            );
            eprintln!(
                "index: preparing {current}/{total} ({percent:>3}%) [{}:{}] {}",
                kind.as_str(),
                language.as_str(),
                path.display()
            );
        },
        IndexProgress::IndexingFile { current, total, path, language, kind } => {
            let percent = progress_percent(
                u64::try_from(current).unwrap_or(u64::MAX),
                u64::try_from(total).unwrap_or(u64::MAX),
            );
            eprintln!(
                "index: {current}/{total} ({percent:>3}%) [{}:{}] {}",
                kind.as_str(),
                language.as_str(),
                path.display()
            );
        },
        IndexProgress::IndexingGitHistory => {
            eprintln!("index: indexing git history");
        },
        IndexProgress::RebuildingLogicalSymbols => {
            eprintln!("index: rebuilding logical symbols");
        },
        IndexProgress::ResolvingGraph => {
            eprintln!("index: resolving graph edges");
        },
        IndexProgress::SyncingFts => {
            eprintln!("index: syncing SQLite FTS");
        },
        IndexProgress::RebuildingFts => {
            eprintln!("index: rebuilding SQLite FTS");
        },
        IndexProgress::Finished { files } => {
            eprintln!("index: complete ({files} files)");
        },
    }
}

/// Format the one-line SCIP-oracle eval summary. Split out from `print_eval_summary` so the
/// formatting is unit-testable without capturing stdout — the rates and raw counts must stay in the
/// line so `eval` output is greppable.
#[cfg(feature = "eval")]
fn format_oracle_line(oracle: &rag_rat_core::index::oracle::OracleEvalMetrics) -> String {
    format!(
        "eval: oracle precision={:.3} recall={:.3} name_only_recovery={:.3} \
         upgradeable_fraction={:.3} (confirm={} contradict={} upgrade={} external={} \
         covered_calls={} oracle_only={})",
        oracle.precision,
        oracle.recall,
        oracle.name_only_recovery_rate,
        oracle.oracle_upgradeable_fraction,
        oracle.confirmed,
        oracle.contradicted,
        oracle.upgraded,
        oracle.resolved_external,
        oracle.covered_calls,
        oracle.oracle_only_calls,
    )
}

#[cfg(all(test, feature = "eval"))]
mod tests {
    use rag_rat_core::index::oracle::OracleEvalMetrics;

    use super::format_oracle_line;

    #[test]
    fn oracle_line_carries_rates_and_raw_counts() {
        let oracle = OracleEvalMetrics {
            precision: 0.5,
            recall: 0.75,
            name_only_recovery_rate: 1.0,
            oracle_upgradeable_fraction: 0.25,
            confirmed: 3,
            contradicted: 3,
            upgraded: 2,
            resolved_external: 1,
            covered_calls: 12,
            oracle_only_calls: 4,
        };
        let line = format_oracle_line(&oracle);
        assert!(line.starts_with("eval: oracle "));
        assert!(line.contains("precision=0.500"));
        assert!(line.contains("recall=0.750"));
        assert!(line.contains("upgradeable_fraction=0.250"));
        // Raw counts present and greppable.
        assert!(line.contains("confirm=3"));
        assert!(line.contains("contradict=3"));
        assert!(line.contains("upgrade=2"));
        assert!(line.contains("external=1"));
        assert!(line.contains("covered_calls=12"));
        assert!(line.contains("oracle_only=4"));
    }
}
