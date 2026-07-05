use std::collections::HashMap;

use super::super::*;
use crate::config::RemoteEmbeddingConfig;

const RECONCILE_SELECT_ID_BATCH_LIMIT: usize = 900;

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
    ensure_model_manifest(conn)?;
    let active_model_id = active_embedding_model_id(conn)?;
    let model = model(conn, &active_model_id)?;
    let model_version = active_embedding_model_version(conn, &active_model_id)?;
    let embedding_dim = usize::try_from(model.embedding_dim.unwrap_or_default()).unwrap_or(0);
    let batch_size = options
        .batch_size
        .map(usize::try_from)
        .transpose()?
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BATCH_SIZE);
    let max_embedding_chars = options.max_embedding_chars.max(MIN_EMBEDDING_CHARS);
    let started = now_ms();
    set_reconcile_meta(conn, LAST_EMBEDDING_RECONCILE_STARTED_META, &started.to_string())?;
    // Stamp the active repo (V042): `reconcile_attempts` carries `repo_id`, so the attempt is
    // attributed to the repo whose reconcile this is — else the row defaults to the placeholder and
    // the per-repo status read never sees it. Per-call literal prefix so the bound params are
    // unchanged; pre-A5 uses the original 4-column shape. The finalize UPDATE keys the row by its
    // autoincrement `id`, so it needs no repo predicate.
    let (repo_col, repo_val) =
        match crate::index::schema::periphery_repo_scope(conn, "reconcile_attempts")? {
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
    let attempt_id = conn.last_insert_rowid();
    let timer = Instant::now();

    // The reconcile scan identity (model id/version/dim + char cap), built ONCE up front so the
    // ephemeral pending-work check inside `acquire_chunk_embedder` sizes candidates exactly like
    // the embed loop below (which reuses this same `scan`).
    let scan = EmbeddingScan {
        model_id: &active_model_id,
        model_version: &model_version,
        dim: embedding_dim,
        max_embedding_chars,
    };
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

    // SkipEphemeral and NoEphemeralWork are the ONLY paths that return BEFORE the policy scan, and
    // both embed nothing:
    //  - SkipEphemeral: an ephemeral active model on a watcher/maintenance pass
    //    (`provision_remote=false`) whose local `query_endpoint` is absent or UNREACHABLE — defer
    //    incremental embedding to an explicit reconcile. (WITH a REACHABLE `query_endpoint`, that
    //    pass takes the light local-embed path → `Ready`, not here.) Returning here avoids paying
    //    the O(repo) `embedding_policy_skip_summary` just to report a deferral.
    //  - NoEphemeralWork: an explicit provisioning reconcile on an already-current ephemeral model
    //    (never cold-start a paid box for zero work, #330-6). `acquire_chunk_embedder` confirmed
    //    ZERO candidates, so the policy scan would likewise be wasted work.
    // Both carry an empty `skipped_by_policy` (no policy counts on these early-return paths). The
    // NotReady path below DOES report policy skips
    // (`blocked_fastembed_reconcile_still_reports_policy_skips` pins that), so it runs the scan
    // like the Ready path.
    let acquired = match acquired {
        skip @ (ChunkEmbedder::SkipEphemeral | ChunkEmbedder::NoEphemeralWork) => {
            let (status, message) = match skip {
                ChunkEmbedder::SkipEphemeral => (
                    "Blocked",
                    Some(
                        "ephemeral remote embedding needs an explicit `rag-rat reconcile`, or a \
                         REACHABLE local `[remote] query_endpoint` server to embed incremental \
                         edits against (the watcher does not provision a GPU box)"
                            .to_string(),
                    ),
                ),
                // Already current → nothing to embed; no paid box was provisioned.
                _ => ("Current", None),
            };
            let report = ReconcileReport {
                processed_chunks: 0,
                embeddings_written: 0,
                skipped_chunks: 0,
                failed_chunks: 0,
                blocked_chunks: 0,
                model_id: active_model_id.clone(),
                model_version: model_version.clone(),
                embedding_dim,
                batch_size,
                max_embedding_chars,
                forced: options.force,
                changed_first: options.changed_first,
                until_clean: options.until_clean,
                max_seconds: options.max_seconds,
                work_reasons: BTreeMap::new(),
                skipped_by_policy: BTreeMap::new(),
                input_chars: 0,
                truncated_inputs: 0,
                elapsed_ms: 0,
                chunks_per_sec: 0.0,
                chars_per_sec: 0.0,
                avg_chars_per_chunk: 0.0,
                status: status.to_string(),
                message,
            };
            finish_reconcile_attempt(conn, attempt_id, &report)?;
            progress(ReconcileProgress::Started {
                model_id: active_model_id,
                total_chunks: 0,
                batch_size,
            });
            progress(ReconcileProgress::Finished {
                processed_chunks: 0,
                embeddings_written: 0,
                failed_chunks: 0,
                blocked_chunks: 0,
            });
            return Ok(report);
        },
        other => other,
    };

    // Ready / NotReady: BOTH report the per-policy skip counts, so run the O(repo) policy summary
    // now. (SkipEphemeral already returned above without paying it.)
    let skipped_by_policy = embedding_policy_skip_summary(conn, max_embedding_chars)?;
    let skipped_chunks = skipped_by_policy.values().sum();
    let mut report = ReconcileReport {
        processed_chunks: 0,
        embeddings_written: 0,
        skipped_chunks,
        failed_chunks: 0,
        blocked_chunks: 0,
        model_id: active_model_id.clone(),
        model_version: model_version.clone(),
        embedding_dim,
        batch_size,
        max_embedding_chars,
        forced: options.force,
        changed_first: options.changed_first,
        until_clean: options.until_clean,
        max_seconds: options.max_seconds,
        work_reasons: BTreeMap::new(),
        skipped_by_policy,
        input_chars: 0,
        truncated_inputs: 0,
        elapsed_ms: 0,
        chunks_per_sec: 0.0,
        chars_per_sec: 0.0,
        avg_chars_per_chunk: 0.0,
        status: "Current".to_string(),
        message: None,
    };

    // `_provisioned` MUST outlive the embed loop: its `Drop` is the box teardown. Bound at function
    // scope here (not inside the match) so it lives until the function returns.
    let (embedder, _provisioned, remote_config) = match acquired {
        ChunkEmbedder::Ready { embedder, provisioned, remote } => (embedder, provisioned, remote),
        ChunkEmbedder::NotReady(err) => {
            // Surface the cause (e.g. a cookbook provisioning failure with its captured stderr) so
            // a remote outage isn't swallowed; the report keeps the actionable "install" hint AND
            // the policy-skip counts already computed above.
            eprintln!("rag-rat: chunk embedder unavailable: {err:#}");
            report.status = "Blocked".to_string();
            report.message = Some(format!(
                "{active_model_id} model is not ready; run `rag-rat models install \
                 {active_model_id}`"
            ));
            finish_reconcile_attempt(conn, attempt_id, &report)?;
            progress(ReconcileProgress::Started {
                model_id: active_model_id,
                total_chunks: 0,
                batch_size,
            });
            progress(ReconcileProgress::Finished {
                processed_chunks: 0,
                embeddings_written: 0,
                failed_chunks: 0,
                blocked_chunks: 0,
            });
            return Ok(report);
        },
        // SkipEphemeral / NoEphemeralWork already returned above.
        ChunkEmbedder::SkipEphemeral | ChunkEmbedder::NoEphemeralWork => {
            unreachable!("SkipEphemeral / NoEphemeralWork handled before the policy scan")
        },
    };
    let selection_batch_size = remote_config
        .as_deref()
        .map(|remote| remote_reconcile_batch_size(remote, batch_size, options.max_seconds))
        .unwrap_or(batch_size);
    let mut progress_total_chunks = estimated_reconcile_jobs(conn, &scan, &options)?;
    progress(ReconcileProgress::Started {
        model_id: active_model_id.clone(),
        total_chunks: progress_total_chunks,
        batch_size,
    });

    // Ordered candidate ids fetched ONCE (ids only, need-first). The loop walks them with a cursor
    // and loads text per batch, so each chunk's text is read at most once — see
    // `embedding_candidate_ids`. The processed set guards against a chunk being revisited (e.g.
    // under --force, whose ordering does not reflect embedding state).
    let candidate_ids = embedding_candidate_ids(
        conn,
        if options.force { "" } else { scan.model_id },
        options.changed_first,
    )?;
    // One dict decoder for the whole run: each `select_reconcile_batch` loads text for its batch
    // from the compressed `chunk_text` store (#77 Phase 2), and reusing this decoder keeps the dict
    // SELECT + dictionary prep to once per run rather than once per batch.
    let dicts = crate::query::chunk_text_dicts(conn)?;
    let mut decoder = crate::index::text_compression::ChunkTextDecoder::new(&dicts);
    let mut cursor = 0usize;
    let mut processed_ids: HashSet<i64> = HashSet::new();
    let mut remaining = options.limit.map(u64::from);
    loop {
        if remaining == Some(0) {
            break;
        }
        if options.max_seconds.is_some_and(|seconds| timer.elapsed().as_secs() >= seconds) {
            report.status = "Partial".to_string();
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
        // Pull the next ordered embed window while keeping each DB lookup under SQLite's default
        // bind-variable limit. Selected jobs are appended in candidate order, so remote reconcile
        // still hands the embedder one window large enough to fill its HTTP concurrency.
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
            let selected = select_reconcile_batch(conn, &scan, &batch_ids, &options, &mut decoder)?;
            window_jobs.extend(selected.jobs);
        }
        if window_jobs.is_empty() {
            if cursor >= candidate_ids.len() {
                break; // candidate list exhausted
            }
            // Every id in this window was filtered (ineligible/already current); keep walking the
            // rest of the candidate list rather than stopping.
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
            match find_existing_embedding(conn, &active_model_id, &job.input_hash, embedding_dim)? {
                Some(vector) => reused_jobs.push((job, vector)),
                None => to_embed_jobs.push(job),
            }
        }

        if !reused_jobs.is_empty() {
            let (reused_jobs_slice, reused_vectors_slice): (Vec<_>, Vec<_>) =
                reused_jobs.into_iter().unzip();
            // The reused vectors were decoded from the content cache (int8 -> f32); writing them
            // back re-encodes to int8 — a negligible re-quantization (codes shift at most one
            // level), well within the int8 scheme's accepted recall cost.
            write_current_embedding_batch(
                conn,
                embedder.as_ref(),
                &model_version,
                &reused_jobs_slice,
                &reused_vectors_slice,
            )?;
            report.embeddings_written += u64::try_from(reused_jobs_slice.len()).unwrap_or(u64::MAX);
        }

        if !to_embed_jobs.is_empty() {
            let (written, failed) = embed_and_write_jobs(
                conn,
                embedder.as_ref(),
                &model_version,
                to_embed_jobs,
                remote_config.as_deref(),
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
    if report.failed_chunks > 0 {
        report.status = "Failed".to_string();
        report.message =
            Some(format!("{} chunks failed; retry after backoff", report.failed_chunks));
    }
    // Embeddings committed under the active model CONFIRM it as the working choice — clear the
    // provisional flag so a later config-model edit no longer reseeds away from it (that would
    // strand these vectors). The active model is what `embed_and_write_jobs` wrote under (#394).
    if report.embeddings_written > 0 {
        clear_active_embedding_model_provisional(conn)?;
    }
    finalize_reconcile_throughput(&mut report, timer.elapsed().as_millis());

    finish_reconcile_attempt(conn, attempt_id, &report)?;
    progress(ReconcileProgress::Finished {
        processed_chunks: report.processed_chunks,
        embeddings_written: report.embeddings_written,
        failed_chunks: report.failed_chunks,
        blocked_chunks: report.blocked_chunks,
    });
    // `report.status` is the stop-reason reaching here (Current | Partial | Failed — the Blocked /
    // NotReady acquire outcomes returned earlier); `remote` shows whether an offload backend was
    // configured (a local light/incremental pass has none). The
    // active-scope proof for #360 (commit/worktree/view-installed, raw-vs-scoped counts) is a
    // deferred follow-up — it needs a conn-level scope introspection helper.
    tracing::info!(
        target: "rag_rat_core::index::ai::reconcile",
        status = %report.status,
        embedded = report.embeddings_written,
        processed = report.processed_chunks,
        failed = report.failed_chunks,
        remote = remote_config.is_some(),
        "reconcile complete"
    );
    Ok(report)
}

fn remote_reconcile_batch_size(
    remote: &RemoteEmbeddingConfig,
    batch_size: usize,
    max_seconds: Option<u64>,
) -> usize {
    if max_seconds.is_some() {
        return batch_size.max(1);
    }
    let remote_batch_size = (remote.batch_size as usize).max(1);
    let concurrency = remote.bounded_concurrency() as usize;
    batch_size.max(remote_batch_size.saturating_mul(concurrency))
}

struct EmbeddingJobGroup {
    primary: PreparedEmbeddingJob,
    duplicates: Vec<PreparedEmbeddingJob>,
}

impl EmbeddingJobGroup {
    fn input_text(&self) -> &str {
        &self.primary.input_text
    }

    fn jobs(&self) -> impl Iterator<Item = &PreparedEmbeddingJob> {
        std::iter::once(&self.primary).chain(self.duplicates.iter())
    }
}

fn group_embedding_jobs_by_input_hash(jobs: Vec<PreparedEmbeddingJob>) -> Vec<EmbeddingJobGroup> {
    let mut groups: Vec<EmbeddingJobGroup> = Vec::new();
    let mut group_by_input_hash: HashMap<String, usize> = HashMap::new();
    for job in jobs {
        if let Some(&group_idx) = group_by_input_hash.get(&job.input_hash) {
            groups[group_idx].duplicates.push(job);
        } else {
            let group_idx = groups.len();
            group_by_input_hash.insert(job.input_hash.clone(), group_idx);
            groups.push(EmbeddingJobGroup { primary: job, duplicates: Vec::new() });
        }
    }
    groups
}

fn embed_and_write_jobs(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    jobs: Vec<PreparedEmbeddingJob>,
    remote: Option<&RemoteEmbeddingConfig>,
) -> anyhow::Result<(u64, u64)> {
    let groups = group_embedding_jobs_by_input_hash(jobs);
    let texts = groups.iter().map(|group| group.input_text().to_string()).collect::<Vec<_>>();
    match embedder.embed_batch(&texts) {
        Ok(vectors) if vectors.len() == groups.len() => {
            let written =
                write_current_embedding_groups(conn, embedder, model_version, &groups, &vectors)?;
            Ok((written, 0))
        },
        Ok(vectors) => {
            let error = vector_count_error(embedder, vectors.len(), groups.len());
            write_remote_scoped_or_failed(conn, embedder, model_version, &groups, remote, &error)
        },
        Err(err) => {
            let error = err.to_string();
            write_remote_scoped_or_failed(conn, embedder, model_version, &groups, remote, &error)
        },
    }
}

fn write_current_embedding_groups(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    groups: &[EmbeddingJobGroup],
    vectors: &[Vec<f32>],
) -> anyhow::Result<u64> {
    let mut written = 0u64;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let write_result = (|| {
        for (group, vector) in groups.iter().zip(vectors) {
            for job in group.jobs() {
                store_embedding(conn, embedder, model_version, job, vector)?;
                written = written.saturating_add(1);
            }
        }
        Ok(())
    })();
    finish_batch_transaction(conn, write_result)?;
    Ok(written)
}

fn write_failed_embedding_groups(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    groups: &[EmbeddingJobGroup],
    error: &str,
) -> anyhow::Result<u64> {
    let mut failed = 0u64;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let write_result = (|| {
        for group in groups {
            for job in group.jobs() {
                store_failed_embedding(conn, embedder, model_version, job, error)?;
                failed = failed.saturating_add(1);
            }
        }
        Ok(())
    })();
    finish_batch_transaction(conn, write_result)?;
    Ok(failed)
}

fn write_remote_scoped_or_failed(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    groups: &[EmbeddingJobGroup],
    remote: Option<&RemoteEmbeddingConfig>,
    error: &str,
) -> anyhow::Result<(u64, u64)> {
    let Some(remote) = remote else {
        let failed = write_failed_embedding_groups(conn, embedder, model_version, groups, error)?;
        return Ok((0, failed));
    };

    let mut written = 0u64;
    let mut failed = 0u64;
    let mut abort_remaining_error = None::<String>;
    let mut consecutive_endpoint_failures = 0usize;
    for (start, end) in remote_request_group_ranges(groups, remote) {
        let scoped_groups = &groups[start..end];
        if let Some(error) = abort_remaining_error.as_deref() {
            let scoped_failed =
                write_failed_embedding_groups(conn, embedder, model_version, scoped_groups, error)?;
            failed = failed.saturating_add(scoped_failed);
            continue;
        }
        let texts =
            scoped_groups.iter().map(|group| group.input_text().to_string()).collect::<Vec<_>>();
        match embedder.embed_batch(&texts) {
            Ok(vectors) if vectors.len() == scoped_groups.len() => {
                consecutive_endpoint_failures = 0;
                let scoped_written = write_current_embedding_groups(
                    conn,
                    embedder,
                    model_version,
                    scoped_groups,
                    &vectors,
                )?;
                written = written.saturating_add(scoped_written);
            },
            Ok(vectors) => {
                consecutive_endpoint_failures = 0;
                let scoped_error = vector_count_error(embedder, vectors.len(), scoped_groups.len());
                let scoped_failed = write_failed_embedding_groups(
                    conn,
                    embedder,
                    model_version,
                    scoped_groups,
                    &scoped_error,
                )?;
                failed = failed.saturating_add(scoped_failed);
            },
            Err(err) => {
                let scoped_error = err.to_string();
                let scoped_failed = write_failed_embedding_groups(
                    conn,
                    embedder,
                    model_version,
                    scoped_groups,
                    &scoped_error,
                )?;
                failed = failed.saturating_add(scoped_failed);
                match classify_remote_scoped_retry_error(&scoped_error) {
                    RemoteScopedRetryError::AbortImmediately => {
                        abort_remaining_error = Some(scoped_error);
                    },
                    RemoteScopedRetryError::EndpointFailure => {
                        consecutive_endpoint_failures =
                            consecutive_endpoint_failures.saturating_add(1);
                        if consecutive_endpoint_failures
                            >= REMOTE_SCOPED_RETRY_CONSECUTIVE_ENDPOINT_FAILURE_LIMIT
                        {
                            abort_remaining_error = Some(scoped_error);
                        }
                    },
                    RemoteScopedRetryError::Other => {
                        consecutive_endpoint_failures = 0;
                    },
                }
            },
        }
    }
    Ok((written, failed))
}

fn vector_count_error(embedder: &dyn Embedder, got: usize, expected: usize) -> String {
    format!("embedder {} returned {} vectors for {} texts", embedder.model_id(), got, expected)
}

const REMOTE_SCOPED_RETRY_CONSECUTIVE_ENDPOINT_FAILURE_LIMIT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteScopedRetryError {
    AbortImmediately,
    EndpointFailure,
    Other,
}

fn classify_remote_scoped_retry_error(error: &str) -> RemoteScopedRetryError {
    let lower = error.to_ascii_lowercase();
    if lower.contains("connection refused")
        || lower.contains("failed to connect")
        || lower.contains("connect error")
    {
        return RemoteScopedRetryError::AbortImmediately;
    }
    if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("connection reset")
        || lower.contains("connection closed")
        || lower.contains("http status 504")
    {
        return RemoteScopedRetryError::EndpointFailure;
    }
    RemoteScopedRetryError::Other
}

fn remote_request_group_ranges(
    groups: &[EmbeddingJobGroup],
    remote: &RemoteEmbeddingConfig,
) -> Vec<(usize, usize)> {
    let batch_size = (remote.batch_size as usize).max(1);
    let max_batch_chars = remote.max_batch_chars.max(1);
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut chars = 0usize;
    for (idx, group) in groups.iter().enumerate() {
        let text_chars = group.input_text().chars().count();
        let count_full = idx.saturating_sub(start) >= batch_size;
        let chars_full = idx > start && chars.saturating_add(text_chars) > max_batch_chars;
        if count_full || chars_full {
            ranges.push((start, idx));
            start = idx;
            chars = 0;
        }
        chars = chars.saturating_add(text_chars);
    }
    if start < groups.len() {
        ranges.push((start, groups.len()));
    }
    ranges
}

pub(crate) fn embedding_policy_skip_summary(
    conn: &Connection,
    max_embedding_chars: usize,
) -> anyhow::Result<BTreeMap<String, u64>> {
    let mut skipped_by_policy = BTreeMap::new();
    // STREAM the chunks one row at a time, counting skips, instead of materializing every chunk.
    // The previous `current_chunks(conn, None)` loaded ALL chunk rows — including each chunk's full
    // `text` — into a `Vec` just to produce this count. On a kernel-sized index (~4.2M chunks) that
    // is ~4 GB resident for a summary that keeps nothing per row, and it runs on every reconcile
    // (and `reconcile_plan`), so it dominated `index --full` peak memory. The same
    // `embedding_policy_for_chunk` runs over the same chunks, so the counts are identical.
    //
    // Chunk text comes from the compressed `chunk_text` store (#77 Phase 2); the `chunks.text`
    // column is gone, so INNER JOIN `chunk_text` (every live chunk has one blob). One dict decoder
    // for the whole stream (versions loaded once, reused per row).
    let dicts = crate::query::chunk_text_dicts(conn)?;
    let mut decoder = crate::index::text_compression::ChunkTextDecoder::new(&dicts);
    let mut stmt = conn.prepare(
        "
        SELECT files.path, files.language, files.kind, chunks.chunk_kind, chunks.symbol_path,
               chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        JOIN chunk_text ON chunk_text.chunk_id = chunks.id
        ",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let path = row.get::<_, String>(0)?;
        let language = row.get::<_, String>(1)?;
        let file_kind = row.get::<_, String>(2)?;
        let chunk_kind = row.get::<_, String>(3)?;
        let symbol_path = row.get::<_, Option<String>>(4)?;
        let text = crate::index::text_compression::ChunkTextRow {
            blob: row.get(5)?,
            raw_len: row.get(6)?,
            dict_version: row.get(7)?,
        }
        .resolve(&mut decoder)?;
        let decision = embedding_policy_for_chunk(
            std::path::Path::new(&path),
            &language,
            &file_kind,
            &chunk_kind,
            symbol_path.as_deref(),
            &text,
            max_embedding_chars,
        );
        if !decision.eligible {
            *skipped_by_policy.entry(decision.policy).or_default() += 1;
        }
    }
    Ok(skipped_by_policy)
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
            report.status,
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

#[cfg(test)]
mod freshness_version_tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::embedding_models::{FASTEMBED_MODEL_ID, HASH_MODEL_ID, spec};

    /// One-shot HTTP/1.1 stub replying to the install probe's `/api/embed` with a `dim`-wide
    /// vector.
    fn spawn_embed_stub(dim: usize) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let nums = vec!["0.1"; dim].join(",");
                let body = format!("{{\"data\":[{{\"embedding\":[{nums}],\"index\":0}}]}}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                     {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn fastembed_dim() -> usize {
        spec(FASTEMBED_MODEL_ID).unwrap().dim
    }

    fn schema_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        ensure_model_manifest(&conn).unwrap();
        conn
    }

    fn remote_at(endpoint: &str) -> RemoteEmbeddingConfig {
        RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: crate::config::RemoteBackend::Ollama,
            endpoint: Some(endpoint.to_string()),
            cookbook: None,
            query_endpoint: None,
            auth_env: None,
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: 384_000,
            request_timeout_s: 5,
        }
    }

    struct TimeoutsThenOkEmbedder {
        calls: AtomicUsize,
        dim: usize,
        failures: usize,
    }

    impl Embedder for TimeoutsThenOkEmbedder {
        fn model_id(&self) -> &str {
            FASTEMBED_MODEL_ID
        }

        fn dim(&self) -> usize {
            self.dim
        }

        fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.failures {
                anyhow::bail!("request timed out");
            }
            Ok(vec![vec![0.1; self.dim]; texts.len()])
        }
    }

    struct RecordingEmbedder {
        calls: AtomicUsize,
        request_sizes: Mutex<Vec<usize>>,
        dim: usize,
    }

    impl Embedder for RecordingEmbedder {
        fn model_id(&self) -> &str {
            FASTEMBED_MODEL_ID
        }

        fn dim(&self) -> usize {
            self.dim
        }

        fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.request_sizes.lock().unwrap().push(texts.len());
            Ok(vec![vec![0.1; self.dim]; texts.len()])
        }
    }

    fn read_request_body(stream: &mut TcpStream) -> String {
        // The listener is non-blocking (poll-accept), and on macOS/BSD an accepted socket INHERITS
        // the listener's `O_NONBLOCK` (Linux/Windows do not). On a non-blocking socket `read`
        // returns `WouldBlock` the instant no bytes are buffered — which the body loop
        // below treats as `Err → break`, TRUNCATING a request whose body spans multiple TCP
        // segments (the 1005-item batch). Force the accepted stream back to blocking so
        // `set_read_timeout` (SO_RCVTIMEO) governs the reads and a large body is drained in
        // full.
        stream.set_nonblocking(false).ok();
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n");
            if let Some(end) = header_end {
                let headers = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                let content_len = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let body_start = end + 4;
                while raw.len() < body_start + content_len {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                return String::from_utf8_lossy(&raw[body_start..body_start + content_len])
                    .to_string();
            }
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return String::new(),
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        }
    }

    fn raise_max(max_seen: &AtomicUsize, value: usize) {
        let mut current = max_seen.load(Ordering::SeqCst);
        while value > current {
            match max_seen.compare_exchange(current, value, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }

    fn spawn_reconcile_embed_stub(
        dim: usize,
        max_conns: usize,
        delay: Duration,
    ) -> (String, thread::JoinHandle<()>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::clone(&max_in_flight);
        let handle = thread::spawn(move || {
            let mut workers = Vec::new();
            let mut accepted = 0usize;
            let started = Instant::now();
            let mut last_accept = started;
            while accepted < max_conns
                && started.elapsed() < Duration::from_secs(5)
                && (accepted == 0 || last_accept.elapsed() < Duration::from_millis(500))
            {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        accepted += 1;
                        last_accept = Instant::now();
                        let in_flight = Arc::clone(&in_flight);
                        let max_seen = Arc::clone(&max_seen);
                        workers.push(thread::spawn(move || {
                            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                            raise_max(&max_seen, now);
                            let body = read_request_body(&mut stream);
                            thread::sleep(delay);
                            let inputs = body.matches("path: ").count().max(1);
                            let vector = vec!["0.1"; dim].join(",");
                            let rows = (0..inputs)
                                .map(|i| format!("{{\"embedding\":[{vector}],\"index\":{i}}}"))
                                .collect::<Vec<_>>()
                                .join(",");
                            let response_body = format!("{{\"data\":[{rows}]}}");
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: \
                                 application/json\r\nContent-Length: {}\r\nConnection: \
                                 close\r\n\r\n{response_body}",
                                response_body.len()
                            );
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                            in_flight.fetch_sub(1, Ordering::SeqCst);
                        }));
                    },
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    },
                    Err(_) => break,
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        (format!("http://127.0.0.1:{port}"), handle, max_in_flight)
    }

    fn spawn_selective_failure_embed_stub(
        dim: usize,
        max_conns: usize,
        fail_marker: &'static str,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let mut workers = Vec::new();
            let mut accepted = 0usize;
            let started = Instant::now();
            let mut last_accept = started;
            while accepted < max_conns
                && started.elapsed() < Duration::from_secs(5)
                && (accepted == 0 || last_accept.elapsed() < Duration::from_millis(500))
            {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        accepted += 1;
                        last_accept = Instant::now();
                        workers.push(thread::spawn(move || {
                            let body = read_request_body(&mut stream);
                            let response = if body.contains(fail_marker) {
                                let response_body = "{\"error\":\"transient\"}";
                                format!(
                                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: \
                                     application/json\r\nContent-Length: {}\r\nConnection: \
                                     close\r\n\r\n{response_body}",
                                    response_body.len()
                                )
                            } else {
                                let inputs = body.matches("path: ").count().max(1);
                                let vector = vec!["0.1"; dim].join(",");
                                let rows = (0..inputs)
                                    .map(|i| format!("{{\"embedding\":[{vector}],\"index\":{i}}}"))
                                    .collect::<Vec<_>>()
                                    .join(",");
                                let response_body = format!("{{\"data\":[{rows}]}}");
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: \
                                     application/json\r\nContent-Length: {}\r\nConnection: \
                                     close\r\n\r\n{response_body}",
                                    response_body.len()
                                )
                            };
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                        }));
                    },
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    },
                    Err(_) => break,
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn seed_embedding_chunk(conn: &Connection, i: i64) -> i64 {
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms)
             VALUES (?1, 'rust', 'source', ?2, 0, 0)",
            params![format!("src/file_{i}.rs"), format!("sha-{i}")],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();
        let text = format!(
            "pub fn item_{i}(input: usize) -> usize {{\n    let mut value = input + {i};\n    for \
             step in 0..16 {{ value = value.saturating_add(step); }}\n    value\n}}"
        );
        conn.execute(
            "INSERT INTO chunks(
                 file_id, chunk_kind, symbol_path, start_byte, end_byte, start_line, end_line,
                 text_hash, source_revision
             )
             VALUES (?1, 'code', ?2, 0, ?3, 1, 3, ?4, ?5)",
            params![
                file_id,
                format!("crate::item_{i}"),
                i64::try_from(text.len()).unwrap(),
                format!("hash-{i}"),
                format!("rev-{i}")
            ],
        )
        .unwrap();
        let chunk_id = conn.last_insert_rowid();
        crate::index::chunk_text_store::seed_chunk_text(conn, chunk_id, &text).unwrap();
        chunk_id
    }

    fn prepared_job(chunk_id: i64, i: i64) -> PreparedEmbeddingJob {
        let input_text = format!("path: src/file_{i}.rs\n\npub fn item_{i}() {{}}");
        PreparedEmbeddingJob {
            id: chunk_id,
            text_hash: format!("hash-{i}"),
            input_hash: format!("input-hash-{i}"),
            input_chars: input_text.chars().count(),
            input_text,
            input_truncated: false,
            policy: "Embed".to_string(),
            priority: 0,
            reason: ReconcileReason::Missing,
        }
    }

    fn active_version(conn: &Connection) -> String {
        repo_meta(conn, ACTIVE_EMBEDDING_MODEL_VERSION_META).unwrap().unwrap()
    }

    #[test]
    fn install_with_remote_toggles_the_row_to_ollama_and_stamps_the_remote_key() {
        // #317 rework: a remote block serves the SELECTED model over Ollama — the SAME ai_models
        // row toggles its runtime to `ollama`, and the freshness key is the remote (not
        // local) version.
        let (url, handle) = spawn_embed_stub(fastembed_dim());
        let conn = schema_conn();
        let remote = remote_at(&url);

        let model =
            install_model(&conn, FASTEMBED_MODEL_ID, Some(&remote)).expect("ollama installs");
        handle.join().unwrap();

        assert_eq!(model.runtime, "ollama", "the row's runtime toggled to ollama");
        let spec = spec(FASTEMBED_MODEL_ID).unwrap();
        assert_eq!(active_version(&conn), remote_freshness_version(spec, &remote));
        assert_ne!(
            active_version(&conn),
            spec.version,
            "remote key differs from the local version"
        );
    }

    #[test]
    fn remote_reconcile_accumulates_enough_work_to_fill_concurrent_embedder_window() {
        let conn = schema_conn();
        let spec = spec(FASTEMBED_MODEL_ID).unwrap();
        for i in 0..4 {
            seed_embedding_chunk(&conn, i);
        }
        let (url, handle, max_in_flight) =
            spawn_reconcile_embed_stub(spec.dim, 4, Duration::from_millis(150));
        let mut remote = remote_at(&url);
        remote.batch_size = 1;
        remote.concurrency = 4;
        set_active_remote_config(&conn, &remote).unwrap();
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
             'ollama'
             WHERE model_id = ?1",
            params![FASTEMBED_MODEL_ID, i64::try_from(spec.dim).unwrap()],
        )
        .unwrap();
        set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
        set_repo_meta(
            &conn,
            ACTIVE_EMBEDDING_MODEL_VERSION_META,
            &remote_freshness_version(spec, &remote),
        )
        .unwrap();

        let report = reconcile_with_options_progress(
            &conn,
            ReconcileOptions { batch_size: Some(1), ..ReconcileOptions::default() },
            |_| {},
        )
        .unwrap();
        handle.join().unwrap();

        assert_eq!(report.batch_size, 1, "public/report batch size is preserved");
        assert_eq!(report.embeddings_written, 4);
        assert_eq!(report.failed_chunks, 0);
        assert!(
            max_in_flight.load(Ordering::SeqCst) > 1,
            "remote reconcile should hand multiple ordered texts to one concurrent embedder call"
        );
    }

    #[test]
    fn remote_reconcile_chunks_large_selection_below_sqlite_bind_limit() {
        let conn = schema_conn();
        let spec = spec(FASTEMBED_MODEL_ID).unwrap();
        for i in 0..1005 {
            seed_embedding_chunk(&conn, i);
        }
        let (url, handle, _) = spawn_reconcile_embed_stub(spec.dim, 1, Duration::ZERO);
        let mut remote = remote_at(&url);
        remote.batch_size = 4096;
        remote.concurrency = 32;
        set_active_remote_config(&conn, &remote).unwrap();
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
             'ollama'
             WHERE model_id = ?1",
            params![FASTEMBED_MODEL_ID, i64::try_from(spec.dim).unwrap()],
        )
        .unwrap();
        set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
        set_repo_meta(
            &conn,
            ACTIVE_EMBEDDING_MODEL_VERSION_META,
            &remote_freshness_version(spec, &remote),
        )
        .unwrap();

        let report = reconcile_with_options_progress(
            &conn,
            ReconcileOptions { batch_size: Some(64), ..ReconcileOptions::default() },
            |_| {},
        )
        .unwrap();
        handle.join().unwrap();

        assert_eq!(report.embeddings_written, 1005);
        assert_eq!(report.failed_chunks, 0);
    }

    #[test]
    fn remote_reconcile_scopes_request_failure_to_remote_request_batch() {
        let conn = schema_conn();
        let spec = spec(FASTEMBED_MODEL_ID).unwrap();
        for i in 0..4 {
            seed_embedding_chunk(&conn, i);
        }
        let (url, handle) = spawn_selective_failure_embed_stub(spec.dim, 8, "item_2");
        let mut remote = remote_at(&url);
        remote.batch_size = 1;
        remote.concurrency = 4;
        set_active_remote_config(&conn, &remote).unwrap();
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
             'ollama'
             WHERE model_id = ?1",
            params![FASTEMBED_MODEL_ID, i64::try_from(spec.dim).unwrap()],
        )
        .unwrap();
        set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
        set_repo_meta(
            &conn,
            ACTIVE_EMBEDDING_MODEL_VERSION_META,
            &remote_freshness_version(spec, &remote),
        )
        .unwrap();

        let report = reconcile_with_options_progress(
            &conn,
            ReconcileOptions { batch_size: Some(1), ..ReconcileOptions::default() },
            |_| {},
        )
        .unwrap();
        handle.join().unwrap();

        assert_eq!(report.embeddings_written, 3);
        assert_eq!(report.failed_chunks, 1);
        let failed_rows: i64 = conn
            .query_row("SELECT count(*) FROM chunk_embeddings WHERE status = 'Failed'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let current_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM chunk_embeddings WHERE status = 'Current'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(failed_rows, 1);
        assert_eq!(current_rows, 3);
    }

    #[test]
    fn remote_reconcile_keeps_caller_batch_size_when_time_bounded() {
        let mut remote = remote_at("http://localhost:11434");
        remote.batch_size = 256;
        remote.concurrency = 32;

        assert_eq!(remote_reconcile_batch_size(&remote, 8, Some(1)), 8);
        assert_eq!(remote_reconcile_batch_size(&remote, 8, None), 8192);
    }

    #[test]
    fn remote_scoped_retry_classifies_endpoint_failures() {
        for error in ["connection refused", "failed to connect", "connect error"] {
            assert_eq!(
                classify_remote_scoped_retry_error(error),
                RemoteScopedRetryError::AbortImmediately,
                "{error}"
            );
        }
        for error in [
            "request timed out",
            "timeout",
            "connection reset",
            "connection closed",
            "http status 504: gateway timeout",
        ] {
            assert_eq!(
                classify_remote_scoped_retry_error(error),
                RemoteScopedRetryError::EndpointFailure,
                "{error}"
            );
        }
        for error in ["http status 500: transient", "embedder model returned 2 vectors for 3 texts"]
        {
            assert_eq!(
                classify_remote_scoped_retry_error(error),
                RemoteScopedRetryError::Other,
                "{error}"
            );
        }
    }

    #[test]
    fn remote_scoped_retry_keeps_later_ranges_after_one_timeout() {
        let conn = schema_conn();
        let jobs = (0..3)
            .map(|i| {
                let chunk_id = seed_embedding_chunk(&conn, i);
                prepared_job(chunk_id, i)
            })
            .collect::<Vec<_>>();
        let mut remote = remote_at("http://localhost:11434");
        remote.batch_size = 1;
        remote.max_batch_chars = usize::MAX;
        let embedder = TimeoutsThenOkEmbedder {
            calls: AtomicUsize::new(0),
            dim: spec(FASTEMBED_MODEL_ID).unwrap().dim,
            failures: 1,
        };
        let groups = group_embedding_jobs_by_input_hash(jobs);

        let (written, failed) = write_remote_scoped_or_failed(
            &conn,
            &embedder,
            "test-version",
            &groups,
            Some(&remote),
            "initial failure",
        )
        .unwrap();

        assert_eq!(written, 2);
        assert_eq!(failed, 1);
        assert_eq!(
            embedder.calls.load(Ordering::SeqCst),
            3,
            "a single timeout should not skip later request ranges"
        );
        let failed_rows: i64 = conn
            .query_row("SELECT count(*) FROM chunk_embeddings WHERE status = 'Failed'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let current_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM chunk_embeddings WHERE status = 'Current'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(failed_rows, 1);
        assert_eq!(current_rows, 2);
    }

    #[test]
    fn remote_scoped_retry_aborts_after_repeated_endpoint_failures() {
        let conn = schema_conn();
        let jobs = (0..4)
            .map(|i| {
                let chunk_id = seed_embedding_chunk(&conn, i);
                prepared_job(chunk_id, i)
            })
            .collect::<Vec<_>>();
        let mut remote = remote_at("http://localhost:11434");
        remote.batch_size = 1;
        remote.max_batch_chars = usize::MAX;
        let embedder = TimeoutsThenOkEmbedder {
            calls: AtomicUsize::new(0),
            dim: spec(FASTEMBED_MODEL_ID).unwrap().dim,
            failures: usize::MAX,
        };
        let groups = group_embedding_jobs_by_input_hash(jobs);

        let (written, failed) = write_remote_scoped_or_failed(
            &conn,
            &embedder,
            "test-version",
            &groups,
            Some(&remote),
            "initial failure",
        )
        .unwrap();

        assert_eq!(written, 0);
        assert_eq!(failed, 4);
        assert_eq!(
            embedder.calls.load(Ordering::SeqCst),
            REMOTE_SCOPED_RETRY_CONSECUTIVE_ENDPOINT_FAILURE_LIMIT,
            "repeated timeout-like failures should stop before serially retrying every range"
        );
    }

    #[test]
    fn embed_and_write_jobs_reuses_same_window_duplicate_input_hashes() {
        let conn = schema_conn();
        let first_chunk_id = seed_embedding_chunk(&conn, 0);
        let duplicate_chunk_id = seed_embedding_chunk(&conn, 1);
        let first_job = prepared_job(first_chunk_id, 0);
        let mut duplicate_job = prepared_job(duplicate_chunk_id, 1);
        duplicate_job.input_hash = first_job.input_hash.clone();
        duplicate_job.input_text = first_job.input_text.clone();
        let embedder = RecordingEmbedder {
            calls: AtomicUsize::new(0),
            request_sizes: Mutex::new(Vec::new()),
            dim: spec(FASTEMBED_MODEL_ID).unwrap().dim,
        };
        let mut remote = remote_at("http://localhost:11434");
        remote.batch_size = 1;
        remote.concurrency = 4;

        let (written, failed) = embed_and_write_jobs(
            &conn,
            &embedder,
            "test-version",
            vec![first_job, duplicate_job],
            Some(&remote),
        )
        .unwrap();

        assert_eq!(written, 2);
        assert_eq!(failed, 0);
        assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *embedder.request_sizes.lock().unwrap(),
            vec![1],
            "duplicate input_hashes in one reconcile window should issue one embed text"
        );
        let current_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM chunk_embeddings WHERE status = 'Current'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(current_rows, 2);
    }

    #[test]
    fn embedding_cache_reuse_survives_chunk_deletion_and_gc_respects_grace() {
        // #357: the content-addressed embedding_cache decouples the vector from chunk_id, so a
        // reindex/branch-switch that deletes a chunk does NOT lose its vector.
        let conn = schema_conn();
        let dim = spec(FASTEMBED_MODEL_ID).unwrap().dim;
        let embedder = RecordingEmbedder {
            calls: AtomicUsize::new(0),
            request_sizes: Mutex::new(Vec::new()),
            dim,
        };
        let mut remote = remote_at("http://localhost:11434");
        remote.batch_size = 8;

        // Embed a chunk → writes chunk_embeddings AND the content-addressed embedding_cache.
        let chunk_id = seed_embedding_chunk(&conn, 0);
        let job = prepared_job(chunk_id, 0);
        let input_hash = job.input_hash.clone();
        let (written, failed) =
            embed_and_write_jobs(&conn, &embedder, "v", vec![job], Some(&remote)).unwrap();
        assert_eq!((written, failed), (1, 0));
        assert!(
            find_existing_embedding(&conn, embedder.model_id(), &input_hash, dim)
                .unwrap()
                .is_some(),
            "embedding_cache is populated on write"
        );

        // REINDEX: deleting the chunk cascade-deletes its chunk_embeddings row (the pre-fix
        // behavior that lost the vector). The content-addressed cache is NOT chunk-scoped.
        conn.execute("DELETE FROM chunks WHERE id = ?1", params![chunk_id]).unwrap();
        let live: i64 =
            conn.query_row("SELECT COUNT(*) FROM chunk_embeddings", [], |r| r.get(0)).unwrap();
        assert_eq!(live, 0, "chunk deletion cascade-deleted the embedding");
        assert!(
            find_existing_embedding(&conn, embedder.model_id(), &input_hash, dim)
                .unwrap()
                .is_some(),
            "reuse survives reindex: the vector is still found in the durable cache"
        );

        // GC keeps a recently-used vector even with no live chunk (fast branch switch-back)...
        assert_eq!(
            prune_embedding_cache_unreferenced(&conn).unwrap(),
            0,
            "recently-used unreferenced entry is kept within the grace"
        );
        // ...and prunes it once it is past the grace with no live chunk referencing it.
        conn.execute("UPDATE embedding_cache SET last_used_at_ms = 0", []).unwrap();
        assert_eq!(
            prune_embedding_cache_unreferenced(&conn).unwrap(),
            1,
            "stale unreferenced entry is pruned"
        );
        assert!(
            find_existing_embedding(&conn, embedder.model_id(), &input_hash, dim)
                .unwrap()
                .is_none(),
            "pruned entry is no longer reusable"
        );
    }

    #[test]
    fn remote_reconcile_malformed_remote_meta_finishes_blocked_attempt() {
        let conn = schema_conn();
        let spec = spec(FASTEMBED_MODEL_ID).unwrap();
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
             'ollama'
             WHERE model_id = ?1",
            params![FASTEMBED_MODEL_ID, i64::try_from(spec.dim).unwrap()],
        )
        .unwrap();
        set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
        set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_VERSION_META, spec.version).unwrap();
        set_repo_meta(&conn, ACTIVE_EMBEDDING_REMOTE_CONFIG_META, "{not valid json").unwrap();

        let report =
            reconcile_with_options_progress(&conn, ReconcileOptions::default(), |_| {}).unwrap();

        assert_eq!(report.status, "Blocked");
        let attempt_status: String = conn
            .query_row(
                "SELECT status FROM reconcile_attempts ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempt_status, "Blocked");
    }

    #[test]
    fn install_rejects_an_unknown_model_id() {
        // No aliases (#317): an unrecognized selector (e.g. the old `minilm` alias) is rejected —
        // the arg must be a registered model_id (the HF path).
        let conn = schema_conn();
        let err = install_model(&conn, "minilm", None).expect_err("alias is no longer accepted");
        assert!(err.to_string().contains("unknown embedding model"), "{err}");
    }

    #[test]
    fn install_rejects_a_remote_block_for_a_non_transformer_target() {
        // A remote block serves the model over Ollama (transformers only). `models install
        // embedding-hash` with a remote block present must be rejected BEFORE any probe — else the
        // hash row would be marked runtime='ollama' with the served model's vectors under its id.
        let conn = schema_conn();
        let remote = remote_at("http://127.0.0.1:1"); // guard fires before any connection attempt
        let err =
            install_model(&conn, HASH_MODEL_ID, Some(&remote)).expect_err("hash + remote rejected");
        assert!(err.to_string().contains("requires a transformer model"), "{err}");
    }

    #[test]
    fn local_install_clears_a_stale_remote_config_meta() {
        // After an Ollama install persists a remote config, re-installing the model LOCALLY must
        // DELETE that meta — otherwise active_embedder keeps building an OpenAiEmbedder against the
        // dead endpoint. Uses the hash model so the local install is feature-free.
        let conn = schema_conn();
        set_active_remote_config(&conn, &remote_at("http://box:11434")).unwrap();
        assert!(active_remote_config(&conn).unwrap().is_some(), "precondition: remote meta set");
        install_model(&conn, HASH_MODEL_ID, None).unwrap();
        assert!(
            active_remote_config(&conn).unwrap().is_none(),
            "a local install must clear the stale remote-config meta",
        );
    }

    #[test]
    fn legacy_active_ollama_model_is_cleaned_on_manifest_ensure() {
        // An index that had the pre-#317 REMOTE id `ollama-all-minilm` installed + active keeps its
        // `ai_models` row + active-model meta + remote-config meta. That id is gone from the
        // registry (Ollama is now a transport), so without legacy cleanup `active_embedder` bails
        // with "unknown active embedding model" — breaking search/reconcile.
        // `ensure_model_manifest` must drop ALL THREE (row, active meta, remote config) and
        // fall back to hash. Feature-free: the legacy row is seeded by raw SQL (no
        // fastembed/model2vec install), and the fallback is the always-available hash
        // embedder.
        const LEGACY_OLLAMA_ID: &str = "ollama-all-minilm";
        let conn = schema_conn();

        // Mirror a real pre-#317 remote install's DB state: a Ready, active `ai_models` row for the
        // removed id, the active-model meta, and a persisted (secret-free) remote config.
        conn.execute(
            "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, \
             disabled, status, installed_at_ms) VALUES (?1, 'embedding', 384, 'ollama', 1, 0, \
             'Ready', 1)",
            params![LEGACY_OLLAMA_ID],
        )
        .unwrap();
        set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, LEGACY_OLLAMA_ID).unwrap();
        set_active_remote_config(&conn, &remote_at("http://box:11434")).unwrap();
        assert!(!model_manifest_is_current(&conn).unwrap(), "a lingering legacy active id is work");

        ensure_model_manifest(&conn).unwrap();

        // The row, the active-model meta, and the remote config are all gone.
        let row_present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM ai_models WHERE model_id = ?1)",
                params![LEGACY_OLLAMA_ID],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!row_present, "the legacy ai_models row is removed");
        assert_eq!(
            repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META).unwrap(),
            None,
            "active meta cleared"
        );
        assert!(
            active_remote_config(&conn).unwrap().is_none(),
            "the stale remote config is cleared so no OpenAiEmbedder is reconstructed",
        );

        // With no active model + no remote config, the active model falls back to hash. Mark the
        // (always-feature-free) hash row Ready as a normal index would, and assert
        // `active_embedder` resolves it WITHOUT the "unknown active embedding model" error
        // the stale legacy row caused.
        install_model(&conn, HASH_MODEL_ID, None).expect("hash installs");
        let embedder =
            active_embedder(&conn, None).expect("falls back to hash, no unknown-model err");
        assert_eq!(embedder.model_id(), HASH_MODEL_ID, "active embedder falls back to hash");
    }

    #[test]
    fn legacy_active_model_cleanup_clears_the_stale_version_meta() {
        // R3a: a pre-#317 legacy-active model (id removed from the registry) had its freshness
        // version meta stamped. `remove_legacy_models` must clear
        // `ACTIVE_EMBEDDING_MODEL_VERSION_META` too when the legacy id was active — else
        // `active_embedding_model_version(HASH)` would inherit the legacy key (it reads the
        // meta for the active model) and bake new hash embeddings under the wrong
        // `model_version`.
        const LEGACY_OLLAMA_ID: &str = "ollama-all-minilm";
        let conn = schema_conn();
        conn.execute(
            "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, \
             disabled, status, installed_at_ms) VALUES (?1, 'embedding', 384, 'ollama', 1, 0, \
             'Ready', 1)",
            params![LEGACY_OLLAMA_ID],
        )
        .unwrap();
        set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META, LEGACY_OLLAMA_ID).unwrap();
        set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_VERSION_META, "ollama-all-minilm-v1-deadbeef")
            .unwrap();

        ensure_model_manifest(&conn).unwrap();

        // The stale version meta is gone. With the active model now the hash fallback,
        // `active_embedding_model_version(HASH)` falls back to the hash spec's static version — NOT
        // the legacy key.
        assert_eq!(
            repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_VERSION_META).unwrap(),
            None,
            "the legacy freshness-version meta is cleared",
        );
        assert_eq!(
            active_embedding_model_version(&conn, HASH_MODEL_ID).unwrap(),
            spec(HASH_MODEL_ID).unwrap().version,
            "hash fallback gets its OWN version, not the stale legacy key",
        );
    }

    #[test]
    fn activate_model_with_version_writes_all_metas() {
        // R3b centralization: the helper every activation site goes through stamps the active
        // model, its version, AND its provenance — so no site can activate without any of
        // them (the recovery bug for the version; the #394 masquerade bug for provenance).
        let conn = schema_conn();
        activate_model_with_version(&conn, HASH_MODEL_ID, "hash-v1", false).unwrap();
        assert_eq!(
            repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_META).unwrap().as_deref(),
            Some(HASH_MODEL_ID)
        );
        assert_eq!(active_version(&conn), "hash-v1");
        assert!(!active_embedding_model_is_provisional(&conn).unwrap(), "false ⇒ non-provisional");
    }

    // Needs a real fastembed install (the no-default-features CI build bails without the feature);
    // HF-path id resolution is also exercised by the rejection + registry tests, which run
    // everywhere.
    #[cfg(feature = "fastembed")]
    #[test]
    fn install_activates_a_model_by_its_hf_path_id() {
        let conn = schema_conn();
        let model = install_model(&conn, FASTEMBED_MODEL_ID, None).expect("hf-path id installs");
        assert_eq!(model.model_id, FASTEMBED_MODEL_ID);
    }

    // The LOCAL re-install is a real fastembed install — gated for the no-default-features build.
    // (`remote_freshness_version` itself + the endpoint-independence are unit-tested feature-free.)
    #[cfg(feature = "fastembed")]
    #[test]
    fn flipping_remote_to_local_resets_the_freshness_version_to_the_static_version() {
        // Install the model over Ollama (remote key), then re-install it LOCALLY (no remote). The
        // active freshness key must flip to the static `spec.version` — a local↔remote flip is a
        // re-embed, and the meta is the single source the reconcile/search path reads.
        let (url, handle) = spawn_embed_stub(fastembed_dim());
        let conn = schema_conn();
        let remote = remote_at(&url);

        install_model(&conn, FASTEMBED_MODEL_ID, Some(&remote)).expect("ollama installs");
        handle.join().unwrap();
        let remote_version = active_version(&conn);

        // Re-install the SAME model locally (no remote).
        install_model(&conn, FASTEMBED_MODEL_ID, None).expect("local re-install");

        let spec = spec(FASTEMBED_MODEL_ID).unwrap();
        assert_eq!(active_version(&conn), spec.version, "flip to local resets to static version");
        assert_ne!(active_version(&conn), remote_version, "must NOT keep the stale remote key");
    }

    #[test]
    fn the_remote_freshness_key_is_endpoint_independent_across_installs() {
        // Two installs at DIFFERENT endpoints (same server model) must stamp the SAME freshness key
        // — an ephemeral box's per-run URL must not re-embed the whole repo.
        let conn = schema_conn();
        let (url_a, h_a) = spawn_embed_stub(fastembed_dim());
        install_model(&conn, FASTEMBED_MODEL_ID, Some(&remote_at(&url_a))).unwrap();
        h_a.join().unwrap();
        let version_a = active_version(&conn);

        let (url_b, h_b) = spawn_embed_stub(fastembed_dim());
        install_model(&conn, FASTEMBED_MODEL_ID, Some(&remote_at(&url_b))).unwrap();
        h_b.join().unwrap();
        let version_b = active_version(&conn);

        assert_eq!(version_a, version_b, "different endpoints → same freshness key (no re-embed)");
    }

    #[test]
    fn installing_a_local_model_stamps_its_static_version() {
        let conn = schema_conn();
        install_model(&conn, HASH_MODEL_ID, None).expect("hash installs");
        assert_eq!(active_version(&conn), spec(HASH_MODEL_ID).unwrap().version);
    }

    /// Activate an ephemeral remote config WITHOUT provisioning (mark the model Ready + persist a
    /// cookbook remote config + freshness meta), mirroring a real ephemeral install's DB state.
    fn activate_ephemeral(conn: &Connection) {
        activate_ephemeral_with_query_endpoint(conn, Some("http://localhost:11434"));
    }

    /// Activate an ephemeral (`cookbook`) remote config with the given local `query_endpoint`.
    /// `None` models the "no local query server" case whose light/watcher pass defers
    /// (`SkipEphemeral`); `Some(..)` models the local light-embed path.
    fn activate_ephemeral_with_query_endpoint(conn: &Connection, query_endpoint: Option<&str>) {
        let spec = spec(FASTEMBED_MODEL_ID).unwrap();
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2, runtime = \
             'ollama'
             WHERE model_id = ?1",
            params![FASTEMBED_MODEL_ID, i64::try_from(spec.dim).unwrap()],
        )
        .unwrap();
        set_repo_meta(conn, ACTIVE_EMBEDDING_MODEL_META, FASTEMBED_MODEL_ID).unwrap();
        let remote = RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: crate::config::RemoteBackend::Ollama,
            endpoint: None,
            cookbook: Some("@rag-rat/cookbook/modal".to_string()),
            query_endpoint: query_endpoint.map(str::to_string),
            auth_env: None,
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: 384_000,
            request_timeout_s: 5,
        };
        set_active_remote_config(conn, &remote).unwrap();
        set_repo_meta(
            conn,
            ACTIVE_EMBEDDING_MODEL_VERSION_META,
            &remote_freshness_version(spec, &remote),
        )
        .unwrap();
    }

    #[test]
    fn reconcile_skips_ephemeral_chunk_embed_without_provision_remote() {
        // The watcher/maintenance pass (`provision_remote: false`) on an ephemeral model with NO
        // local `query_endpoint` server must NOT cold-start a cookbook box — it returns Blocked
        // with a "needs explicit reconcile" message and never spawns a subprocess. (No
        // cookbook is actually runnable here, so a provisioning attempt would error/hang;
        // the skip is what keeps this test fast + offline.) With a `query_endpoint` set,
        // the light path embeds locally instead — see
        // `light_pass_with_query_endpoint_embeds_locally_without_provisioning`.
        let conn = schema_conn();
        activate_ephemeral_with_query_endpoint(&conn, None);

        let report = reconcile_with_options_progress(
            &conn,
            ReconcileOptions { provision_remote: false, ..ReconcileOptions::default() },
            |_| {},
        )
        .expect("reconcile returns a report (skips, does not error)");

        assert_eq!(report.status, "Blocked");
        assert_eq!(report.embeddings_written, 0);
        assert!(
            report.message.as_deref().unwrap_or_default().contains("explicit `rag-rat reconcile`"),
            "skip message: {:?}",
            report.message
        );
    }

    /// Build the light-path acquire for an ephemeral active model (mirrors the scan
    /// `reconcile_with_options_progress` builds), on a non-provisioning (watcher) pass.
    fn ephemeral_light_acquire(conn: &Connection) -> ChunkEmbedder {
        let model_id = active_embedding_model_id(conn).unwrap();
        let dim = spec(&model_id).unwrap().dim;
        let version = active_version(conn);
        let scan = EmbeddingScan {
            model_id: &model_id,
            model_version: &version,
            dim,
            max_embedding_chars: 4000,
        };
        acquire_chunk_embedder(conn, None, &scan, &ReconcileOptions {
            provision_remote: false,
            ..ReconcileOptions::default()
        })
    }

    #[test]
    fn light_pass_without_query_endpoint_defers_to_explicit_reconcile() {
        // Ephemeral watcher pass with NO local query server → SkipEphemeral (defer), even with
        // pending work: the "watcher never cold-starts a paid box" guarantee is preserved.
        let conn = schema_conn();
        activate_ephemeral_with_query_endpoint(&conn, None);
        seed_embedding_chunk(&conn, 1);
        assert!(matches!(ephemeral_light_acquire(&conn), ChunkEmbedder::SkipEphemeral));
    }

    #[test]
    fn light_pass_with_reachable_query_endpoint_embeds_locally_single_flight() {
        // Ephemeral watcher pass, `query_endpoint` answers a probe embed → Ready with NO
        // provisioned box: the light path builds the LOCAL query-endpoint embedder (same
        // vector space as the cookbook box, no cold-start), the probe embed succeeds, and
        // concurrency is clamped to SINGLE-FLIGHT so a background edit can't overload the
        // local server.
        let conn = schema_conn();
        let (endpoint, _stub) = spawn_embed_stub(fastembed_dim());
        activate_ephemeral_with_query_endpoint(&conn, Some(&endpoint));
        seed_embedding_chunk(&conn, 1);
        match ephemeral_light_acquire(&conn) {
            ChunkEmbedder::Ready { provisioned: None, remote: Some(r), .. } => {
                assert_eq!(r.concurrency, 1, "light path must be single-flight");
                assert_eq!(
                    r.endpoint.as_deref(),
                    Some(endpoint.as_str()),
                    "embeds against the query_endpoint"
                );
            },
            _ => panic!("expected a local Ready with provisioned=None"),
        }
    }

    #[test]
    fn light_pass_with_unreachable_query_endpoint_defers() {
        // Ephemeral watcher pass, `query_endpoint` set but the server is DOWN (a closed port) → the
        // probe embed's connect is refused, so defer (SkipEphemeral), NOT embed-and-fail into
        // `Failed` chunk_embeddings, and without paying an O(repo) candidate scan first.
        let conn = schema_conn();
        let closed = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener); // the port now refuses connections
            format!("http://127.0.0.1:{port}")
        };
        activate_ephemeral_with_query_endpoint(&conn, Some(&closed));
        seed_embedding_chunk(&conn, 1);
        assert!(matches!(ephemeral_light_acquire(&conn), ChunkEmbedder::SkipEphemeral));
    }

    #[test]
    fn light_pass_with_wrong_model_on_the_endpoint_defers() {
        // The port ACCEPTS connections but the embeddings route returns the WRONG dim (a different
        // service, or the configured model not pulled) — a bare TCP connect would pass, but the
        // probe embed catches the dim mismatch and defers (SkipEphemeral) instead of persisting
        // `Failed` chunk rows.
        let conn = schema_conn();
        let (endpoint, _stub) = spawn_embed_stub(fastembed_dim() + 1); // wrong dim on the route
        activate_ephemeral_with_query_endpoint(&conn, Some(&endpoint));
        seed_embedding_chunk(&conn, 1);
        assert!(matches!(ephemeral_light_acquire(&conn), ChunkEmbedder::SkipEphemeral));
    }

    #[test]
    fn reconcile_does_not_provision_when_an_ephemeral_model_is_already_current() {
        // #330-6: an explicit `rag-rat reconcile` (`provision_remote: true`) on an ephemeral active
        // model that has NOTHING pending must NOT cold-start (and immediately tear down) a paid GPU
        // box. The repo here has ZERO chunks, so there are zero candidates. The cookbook spec
        // (`@rag-rat/cookbook/modal`) is NOT runnable in the test env, so IF provisioning were
        // attempted it would fail → a "Blocked" / error report. A clean "Current" report with no
        // embeddings is the proof that `acquire_chunk_embedder` short-circuited to
        // `NoEphemeralWork` BEFORE provisioning. (Contrast the `provision_remote: false`
        // skip test above, which returns "Blocked".)
        let conn = schema_conn();
        activate_ephemeral(&conn);

        let report = reconcile_with_options_progress(
            &conn,
            ReconcileOptions { provision_remote: true, ..ReconcileOptions::default() },
            |_| {},
        )
        .expect("reconcile returns a report (no provision attempt, no error)");

        assert_eq!(report.status, "Current", "no pending work → Current, not Blocked: {report:?}");
        assert_eq!(report.embeddings_written, 0);
        assert_eq!(report.processed_chunks, 0);
        assert!(
            report.message.is_none(),
            "no-op reconcile carries no failure message: {:?}",
            report.message
        );
    }
}
