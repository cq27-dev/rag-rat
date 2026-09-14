use std::ops::ControlFlow;

use rag_rat_base::config::RemoteEmbeddingConfig;

use super::super::*;
use super::{batch_write, policy_scan};

const RECONCILE_SELECT_ID_BATCH_LIMIT: usize = 900;

#[cfg(test)]
pub(crate) fn reconcile(
    conn: &Connection,
    limit: Option<u32>,
    batch_size: Option<u32>,
) -> anyhow::Result<ReconcileReport> {
    reconcile_with_options_progress(
        conn,
        ReconcileOptions { limit, batch_size, ..ReconcileOptions::default() },
        |_| {},
    )
}

#[cfg(test)]
pub(crate) fn reconcile_with_progress(
    conn: &Connection,
    limit: Option<u32>,
    batch_size: Option<u32>,
    force: bool,
    progress: impl FnMut(ReconcileProgress),
) -> anyhow::Result<ReconcileReport> {
    reconcile_with_options_progress(
        conn,
        ReconcileOptions { limit, batch_size, force, ..ReconcileOptions::default() },
        progress,
    )
}

pub(crate) fn reconcile_with_options_progress(
    conn: &Connection,
    options: ReconcileOptions,
    mut progress: impl FnMut(ReconcileProgress),
) -> anyhow::Result<ReconcileReport> {
    let active = super::status::ActiveEmbeddingModel::resolve(conn)?;
    let active_model_id = active.model.model_id.clone();
    let model_version = active.model_version.clone();
    let embedding_dim = active.dim;
    let batch_size = options
        .batch_size
        .map(usize::try_from)
        .transpose()?
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BATCH_SIZE);
    let max_embedding_chars = options.max_embedding_chars.max(MIN_EMBEDDING_CHARS);
    // The reconcile scan identity (model id/version/dim + char cap), built ONCE up front so the
    // ephemeral pending-work check inside `acquire_chunk_embedder` sizes candidates exactly like
    // the embed loop below (which reuses this same `scan`).
    // `stamped_policy` is re-derived AFTER the self-heal below (a stale/absent stamp is repaired +
    // re-certified there); this initial value only governs the pre-heal preflight estimate.
    let mut scan = active.scan(conn, max_embedding_chars)?;
    let preflight_estimated_jobs = if batch_write::automatic_reconcile_can_skip_noop(conn, &options)
    {
        match estimated_reconcile_jobs(conn, &scan, &options) {
            Ok(0) =>
                return Ok(batch_write::empty_current_reconcile_report(
                    active_model_id,
                    model_version,
                    embedding_dim,
                    batch_size,
                    max_embedding_chars,
                    &options,
                )),
            Ok(jobs) => Some(jobs),
            Err(_) => None,
        }
    } else {
        None
    };
    let attempt_id = record_attempt_start(conn, &options, batch_size)?;
    let timer = Instant::now();
    // The chunk-embed embedder. For an EPHEMERAL active model on a provisioning reconcile, this
    // PROVISIONS a cookbook box (held by `_provisioned` for the whole loop — its `Drop` tears the
    // box down on success/error/panic) — but only AFTER confirming there's pending work, so a no-op
    // reconcile never cold-starts a paid box (#330-6). Otherwise it's `active_embedder`
    // (connect/local). The `acquire_chunk_embedder` result distinguishes ready / skip-ephemeral /
    // no-ephemeral-work / not-ready.
    //
    // Acquire FIRST, then decide whether to do any work — `embedding_policy_skip_summary` streams +
    // decompresses EVERY chunk (O(repo)). The skip/not-ready paths embed nothing, so they must NOT
    // pay that scan: a watcher pass with an ephemeral active model + `provision_remote=false` fires
    // on every file change, and running a full-repo scan per pass just to return "Blocked" is pure
    // waste. Only the Ready path (which actually walks the candidates) runs the policy summary.
    let acquired = acquire_chunk_embedder(conn, options.intra_threads, &scan, &options);

    let mut pass = match begin_embed_pass(
        conn,
        &mut scan,
        acquired,
        EmbedPassStart { options: &options, attempt_id, batch_size },
        &mut progress,
    )? {
        ControlFlow::Break(report) => return Ok(report),
        ControlFlow::Continue(pass) => pass,
    };
    let selection_batch_size = pass
        .remote
        .as_deref()
        .map(|remote| {
            batch_write::remote_reconcile_batch_size(remote, batch_size, options.max_seconds)
        })
        .unwrap_or(batch_size);
    let progress_total_chunks = match preflight_estimated_jobs.or(pass.estimated_jobs) {
        Some(jobs) => jobs,
        None => estimated_reconcile_jobs(conn, &scan, &options)?,
    };
    progress(ReconcileProgress::Started {
        model_id: active_model_id.clone(),
        total_chunks: progress_total_chunks,
        batch_size,
    });
    EmbedPass {
        conn,
        scan: &scan,
        options: &options,
        embedder: pass.embedder.as_ref(),
        remote: pass.remote.as_deref(),
        selection_batch_size,
        timer,
    }
    .drain_candidate_windows(&mut pass.report, progress_total_chunks, &mut progress)?;
    if pass.report.failed_chunks > 0 {
        pass.report.status = ReconcileStatus::Failed;
        pass.report.message =
            Some(format!("{} chunks failed; retry after backoff", pass.report.failed_chunks));
    }
    // Embeddings committed under the active model CONFIRM it as the working choice — clear the
    // provisional flag so a later config-model edit no longer reseeds away from it (that would
    // strand these vectors). The active model is what `embed_and_write_jobs` wrote under (#394).
    if pass.report.embeddings_written > 0 {
        clear_active_embedding_model_provisional(conn)?;
    }
    finalize_reconcile_throughput(&mut pass.report, timer.elapsed().as_millis());

    finish_reconcile_attempt(conn, attempt_id, &pass.report)?;
    progress(ReconcileProgress::Finished {
        processed_chunks: pass.report.processed_chunks,
        embeddings_written: pass.report.embeddings_written,
        failed_chunks: pass.report.failed_chunks,
        blocked_chunks: pass.report.blocked_chunks,
    });
    // `pass.report.status` is the stop-reason reaching here (Current | Partial | Failed — the
    // Blocked / NotReady acquire outcomes returned earlier); `remote` shows whether an offload
    // backend was configured (a local light/incremental pass has none). The
    // active-scope proof for #360 (commit/worktree/view-installed, raw-vs-scoped counts) is a
    // deferred follow-up — it needs a conn-level scope introspection helper.
    tracing::info!(
        target: "rag_rat_core::index::ai::reconcile",
        status = %pass.report.status.as_db_str(),
        embedded = pass.report.embeddings_written,
        processed = pass.report.processed_chunks,
        failed = pass.report.failed_chunks,
        remote = pass.remote.is_some(),
        "reconcile complete"
    );
    Ok(pass.report)
}

/// Record the run-start meta and insert this reconcile's `Running` attempt row, returning its id
/// for [`finish_reconcile_attempt`].
fn record_attempt_start(
    conn: &Connection,
    options: &ReconcileOptions,
    batch_size: usize,
) -> anyhow::Result<i64> {
    let started = now_ms();
    set_reconcile_meta(conn, LAST_EMBEDDING_RECONCILE_STARTED_META, &started.to_string())?;
    // Stamp the active repo (V042): `reconcile_attempts` carries `repo_id`, so the attempt is
    // attributed to the repo whose reconcile this is — else the row defaults to the placeholder and
    // the per-repo status read never sees it. Per-call literal prefix so the bound params are
    // unchanged; pre-A5 uses the original 4-column shape. The finalize UPDATE keys the row by its
    // autoincrement `id`, so it needs no repo predicate.
    let (repo_col, repo_val) =
        match rag_rat_db::schema::periphery_repo_scope(conn, "reconcile_attempts")? {
            Some(repo_id) =>
                ("repo_id, ".to_string(), format!("'{}', ", repo_id.replace('\'', "''"))),
            None => (String::new(), String::new()),
        };
    conn.execute(
        &format!(
            "INSERT INTO reconcile_attempts({repo_col}started_at_ms, limit_count, status, \
             batch_size) VALUES ({repo_val}?1, ?2, 'Running', ?3)"
        ),
        params![
            started,
            options.limit.map(i64::from),
            i64::try_from(batch_size).unwrap_or(i64::MAX)
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Ready and NotReady both report the per-policy skip counts, so run the repo-wide policy summary
/// for them. (SkipEphemeral / NoEphemeralWork return without paying it.) Self-heal the policy
/// column first so this summary and every later reconcile/plan take the fast GROUP BY path; the
/// heal may have just repaired + re-certified the stamp (the stale/absent-stamp upgrade path), so
/// re-derive `scan`'s certification NOW so the embed loop reads the healed columns instead of
/// re-parsing every candidate FromText for the whole run — the first post-upgrade reconcile is
/// exactly the large-repo case this fast path targets (#725). Returns the in-progress report.
fn heal_policy_and_report_skips(
    conn: &Connection,
    scan: &mut EmbeddingScan<'_>,
    options: &ReconcileOptions,
    batch_size: usize,
) -> anyhow::Result<ReconcileReport> {
    policy_scan::maybe_heal_embedding_policy(conn, scan.max_embedding_chars);
    scan.stamped_policy = policy_scan::stamped_policy_certified(conn, scan.max_embedding_chars)?;
    let skipped_by_policy =
        policy_scan::embedding_policy_skip_summary(conn, scan.max_embedding_chars)?;
    Ok(ReconcileReport {
        skipped_chunks: skipped_by_policy.values().sum(),
        skipped_by_policy,
        ..batch_write::empty_current_reconcile_report(
            scan.model_id.to_string(),
            scan.model_version.to_string(),
            scan.dim,
            batch_size,
            scan.max_embedding_chars,
            options,
        )
    })
}

/// Close an attempt that embedded nothing: persist `report`, then emit the empty Started/Finished
/// progress pair every reconcile path reports.
fn finish_attempt_without_embedding(
    conn: &Connection,
    attempt_id: i64,
    report: ReconcileReport,
    progress: &mut impl FnMut(ReconcileProgress),
) -> anyhow::Result<ReconcileReport> {
    finish_reconcile_attempt(conn, attempt_id, &report)?;
    progress(ReconcileProgress::Started {
        model_id: report.model_id.clone(),
        total_chunks: 0,
        batch_size: report.batch_size,
    });
    progress(ReconcileProgress::Finished {
        processed_chunks: 0,
        embeddings_written: 0,
        failed_chunks: 0,
        blocked_chunks: 0,
    });
    Ok(report)
}

/// The stable inputs of one reconcile's embed loop.
struct EmbedPass<'a, 's> {
    conn: &'a Connection,
    scan: &'a EmbeddingScan<'s>,
    options: &'a ReconcileOptions,
    embedder: &'a dyn Embedder,
    remote: Option<&'a RemoteEmbeddingConfig>,
    selection_batch_size: usize,
    timer: Instant,
}

impl EmbedPass<'_, '_> {
    /// Walk the ordered candidate list in embed windows until the limit, the time budget, or the
    /// list runs out, folding each window's outcome into `report` and emitting a Batch progress
    /// event per window.
    fn drain_candidate_windows(
        &self,
        report: &mut ReconcileReport,
        mut progress_total_chunks: u64,
        progress: &mut impl FnMut(ReconcileProgress),
    ) -> anyhow::Result<()> {
        let Self { conn, scan, options, embedder, remote, selection_batch_size, timer } = *self;
        // Ordered candidate ids fetched ONCE (ids only, need-first). The loop walks them with a
        // cursor and loads text per batch, so each chunk's text is read at most once — see
        // `embedding_candidate_ids`. The processed set guards against a chunk being revisited
        // (e.g. under --force, whose ordering does not reflect embedding state).
        let candidate_ids = embedding_candidate_ids(
            conn,
            if options.force { "" } else { scan.model_id },
            options.changed_first,
        )?;
        // Snapshot the scoped file metadata ONCE for the whole loop, alongside `candidate_ids`.
        // The per-batch chunk query joins this indexed temp table instead of probing the live
        // `UNION ALL` scope view per chunk row (which is O(files) each) — see
        // `snapshot_reconcile_scope_files`.
        snapshot_reconcile_scope_files(conn)?;
        // One dict decoder for the whole run: each `select_reconcile_batch` loads text for its
        // batch from the compressed `chunk_text` store (#77 Phase 2), and reusing this decoder
        // keeps the dict SELECT + dictionary prep to once per run rather than once per batch.
        let dicts = rag_rat_query::chunk_text_dicts(conn)?;
        let mut decoder = rag_rat_db::text_compression::ChunkTextDecoder::new(&dicts);
        let mut cursor = 0usize;
        let mut processed_ids: HashSet<i64> = HashSet::new();
        let mut remaining = options.limit.map(u64::from);
        loop {
            if remaining == Some(0) {
                break;
            }
            if options.max_seconds.is_some_and(|seconds| timer.elapsed().as_secs() >= seconds) {
                report.status = ReconcileStatus::Partial;
                report.message = Some(format!(
                    "max_seconds={} reached; rerun reconcile to continue",
                    options.max_seconds.unwrap_or_default()
                ));
                break;
            }
            let window_limit = remaining
                .map(|value| value.min(u64::try_from(selection_batch_size).unwrap_or(u64::MAX)))
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(selection_batch_size);
            // Pull the next ordered embed window while keeping each DB lookup under SQLite's
            // default bind-variable limit. Selected jobs are appended in candidate order, so
            // remote reconcile still hands the embedder one window large enough to fill its HTTP
            // concurrency.
            let mut window_jobs = Vec::new();
            let mut ids_seen = 0usize;
            while cursor < candidate_ids.len() && ids_seen < window_limit {
                let id_limit = RECONCILE_SELECT_ID_BATCH_LIMIT.min(window_limit - ids_seen);
                let mut batch_ids = Vec::with_capacity(id_limit);
                while cursor < candidate_ids.len()
                    && batch_ids.len() < id_limit
                    && ids_seen < window_limit
                {
                    let id = candidate_ids[cursor];
                    cursor += 1;
                    ids_seen = ids_seen.saturating_add(1);
                    if !processed_ids.contains(&id) {
                        batch_ids.push(id);
                    }
                }
                if batch_ids.is_empty() {
                    break;
                }
                let selected =
                    select_reconcile_batch(conn, scan, &batch_ids, options, &mut decoder)?;
                window_jobs.extend(selected.jobs);
            }
            if window_jobs.is_empty() {
                if cursor >= candidate_ids.len() {
                    break; // candidate list exhausted
                }
                // Every id in this window was filtered (ineligible/already current); keep walking
                // the rest of the candidate list rather than stopping.
                continue;
            }
            for job in &window_jobs {
                processed_ids.insert(job.id);
                *report.work_reasons.entry(job.reason.as_str().to_string()).or_default() += 1;
                report.input_chars = report
                    .input_chars
                    .saturating_add(u64::try_from(job.input_chars).unwrap_or(u64::MAX));
                if job.input_truncated {
                    report.truncated_inputs += 1;
                }
            }
            let jobs_len = window_jobs.len();
            let mut reused_jobs = Vec::new();
            let mut to_embed_jobs = Vec::new();
            for job in window_jobs {
                match find_existing_embedding(conn, scan.model_id, &job.input_hash, scan.dim)? {
                    Some(vector) => reused_jobs.push((job, vector)),
                    None => to_embed_jobs.push(job),
                }
            }

            if !reused_jobs.is_empty() {
                let (reused_jobs_slice, reused_vectors_slice): (Vec<_>, Vec<_>) =
                    reused_jobs.into_iter().unzip();
                // The reused vectors were decoded from the content cache (int8 -> f32); writing
                // them back re-encodes to int8 — a negligible re-quantization (codes shift at most
                // one level), well within the int8 scheme's accepted recall cost.
                write_current_embedding_batch(
                    conn,
                    embedder,
                    scan.model_version,
                    &reused_jobs_slice,
                    &reused_vectors_slice,
                )?;
                report.embeddings_written +=
                    u64::try_from(reused_jobs_slice.len()).unwrap_or(u64::MAX);
            }

            if !to_embed_jobs.is_empty() {
                let (written, failed) = batch_write::embed_and_write_jobs(
                    conn,
                    embedder,
                    scan.model_version,
                    to_embed_jobs,
                    remote,
                )?;
                report.embeddings_written = report.embeddings_written.saturating_add(written);
                report.failed_chunks = report.failed_chunks.saturating_add(failed);
            }
            report.processed_chunks = report
                .embeddings_written
                .saturating_add(report.failed_chunks)
                .saturating_add(report.blocked_chunks);
            if let Some(value) = remaining.as_mut() {
                *value = value.saturating_sub(u64::try_from(jobs_len).unwrap_or(0));
            }
            progress_total_chunks = progress_total_chunks.max(report.processed_chunks);
            progress(ReconcileProgress::Batch {
                processed_chunks: report.embeddings_written
                    + report.failed_chunks
                    + report.blocked_chunks,
                total_chunks: progress_total_chunks,
                embeddings_written: report.embeddings_written,
                failed_chunks: report.failed_chunks,
                blocked_chunks: report.blocked_chunks,
            });
        }
        Ok(())
    }
}

pub(crate) fn finish_reconcile_attempt(
    conn: &Connection,
    attempt_id: i64,
    report: &ReconcileReport,
) -> anyhow::Result<()> {
    let finished = now_ms();
    conn.execute(
        "
        UPDATE reconcile_attempts
        SET finished_at_ms = ?2,
            processed_chunks = ?3,
            embeddings_written = ?4,
            blocked_chunks = ?5,
            status = ?6,
            message = ?7,
            elapsed_ms = ?8,
            input_chars = ?9,
            batch_size = ?10
        WHERE id = ?1
        ",
        params![
            attempt_id,
            finished,
            i64::try_from(report.processed_chunks).unwrap_or(i64::MAX),
            i64::try_from(report.embeddings_written).unwrap_or(i64::MAX),
            i64::try_from(report.blocked_chunks).unwrap_or(i64::MAX),
            report.status.as_db_str(),
            report.message,
            i64::try_from(report.elapsed_ms).unwrap_or(i64::MAX),
            i64::try_from(report.input_chars).unwrap_or(i64::MAX),
            i64::try_from(report.batch_size).unwrap_or(i64::MAX),
        ],
    )?;
    set_reconcile_meta(conn, LAST_EMBEDDING_RECONCILE_FINISHED_META, &finished.to_string())?;
    Ok(())
}

pub(crate) fn finalize_reconcile_throughput(report: &mut ReconcileReport, elapsed_ms: u128) {
    report.elapsed_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);
    let elapsed_secs = (report.elapsed_ms as f64 / 1000.0).max(0.001);
    report.chunks_per_sec = report.embeddings_written as f64 / elapsed_secs;
    report.chars_per_sec = report.input_chars as f64 / elapsed_secs;
    report.avg_chars_per_chunk = if report.embeddings_written > 0 {
        report.input_chars as f64 / report.embeddings_written as f64
    } else {
        0.0
    };
}

struct AcquiredEmbedPass {
    embedder: Box<dyn Embedder>,
    // Keep the provisioned box alive through the orchestrator's embed loop and final reporting.
    _provisioned: Option<ProvisionedBox>,
    remote: Option<Box<RemoteEmbeddingConfig>>,
    estimated_jobs: Option<u64>,
    report: ReconcileReport,
}

struct EmbedPassStart<'a> {
    options: &'a ReconcileOptions,
    attempt_id: i64,
    batch_size: usize,
}

fn begin_embed_pass(
    conn: &Connection,
    scan: &mut EmbeddingScan<'_>,
    acquired: ChunkEmbedder,
    start: EmbedPassStart<'_>,
    progress: &mut impl FnMut(ReconcileProgress),
) -> anyhow::Result<ControlFlow<ReconcileReport, AcquiredEmbedPass>> {
    let EmbedPassStart { options, attempt_id, batch_size } = start;
    let active_model_id = scan.model_id;
    let model_version = scan.model_version;
    let embedding_dim = scan.dim;
    let max_embedding_chars = scan.max_embedding_chars;
    // SkipEphemeral and NoEphemeralWork are the ONLY paths that return BEFORE the policy scan, and
    // both embed nothing:
    //  - SkipEphemeral: an ephemeral active model on a watcher/maintenance pass
    //    (`provision_remote=false`) whose local `query_endpoint` is absent or UNREACHABLE — defer
    //    incremental embedding to an explicit reconcile. (WITH a REACHABLE `query_endpoint`, that
    //    pass takes the light local-embed path → `Ready`, not here.) Returning here avoids paying
    //    the repo-wide `embedding_policy_skip_summary` scan just to report a deferral.
    //  - NoEphemeralWork: an explicit provisioning reconcile on an already-current ephemeral model
    //    (never cold-start a paid box for zero work, #330-6). `acquire_chunk_embedder` confirmed
    //    ZERO candidates, so the policy scan would likewise be wasted work.
    // Both carry an empty `skipped_by_policy` (no policy counts on these early-return paths). The
    // NotReady path below DOES report policy skips
    // (`blocked_fastembed_reconcile_still_reports_policy_skips` pins that), so it runs the scan
    // like the Ready path.
    // The returned pass owns `_provisioned` through the orchestrator scope; its Drop tears down the
    // box.
    match acquired {
        ChunkEmbedder::Ready { embedder, provisioned, remote, estimated_jobs } => {
            let report = heal_policy_and_report_skips(conn, scan, options, batch_size)?;
            Ok(ControlFlow::Continue(AcquiredEmbedPass {
                embedder,
                _provisioned: provisioned,
                remote,
                estimated_jobs,
                report,
            }))
        },
        ChunkEmbedder::NotReady(err) => {
            // Surface the cause (e.g. a cookbook provisioning failure with its captured
            // stderr) so a remote outage isn't swallowed; the report keeps the actionable
            // "install" hint AND the policy-skip counts.
            let mut report = heal_policy_and_report_skips(conn, scan, options, batch_size)?;
            eprintln!("rag-rat: chunk embedder unavailable: {err:#}");
            report.status = ReconcileStatus::Blocked;
            report.message = Some(format!(
                "{active_model_id} model is not ready; run `rag-rat models install \
                 {active_model_id}`"
            ));
            finish_attempt_without_embedding(conn, attempt_id, report, progress)
                .map(ControlFlow::Break)
        },
        skip @ (ChunkEmbedder::SkipEphemeral | ChunkEmbedder::NoEphemeralWork) => {
            // NoEphemeralWork is an EXPLICIT `rag-rat reconcile` with nothing to embed — still
            // a good moment to certify the policy column after a version bump so later
            // plans take the fast path. SkipEphemeral is a FREQUENT watcher/maintenance
            // deferral and must stay cheap: no heal scan. (The heal itself no-ops
            // unless the stamp is stale at the DEFAULT cap.)
            if matches!(skip, ChunkEmbedder::NoEphemeralWork) {
                policy_scan::maybe_heal_embedding_policy(conn, max_embedding_chars);
            }
            let (status, message) = match skip {
                ChunkEmbedder::SkipEphemeral => (
                    ReconcileStatus::Blocked,
                    Some(
                        "ephemeral remote embedding needs an explicit `rag-rat reconcile`, or a \
                         REACHABLE local `[remote] query_endpoint` server to embed incremental \
                         edits against (the watcher does not provision a GPU box)"
                            .to_string(),
                    ),
                ),
                // Already current → nothing to embed; no paid box was provisioned.
                _ => (ReconcileStatus::Current, None),
            };
            let report = ReconcileReport {
                status,
                message,
                ..batch_write::empty_current_reconcile_report(
                    active_model_id.to_string(),
                    model_version.to_string(),
                    embedding_dim,
                    batch_size,
                    max_embedding_chars,
                    options,
                )
            };
            finish_attempt_without_embedding(conn, attempt_id, report, progress)
                .map(ControlFlow::Break)
        },
    }
}

#[cfg(test)]
#[path = "embed_loop_tests.rs"]
mod freshness_version_tests;
