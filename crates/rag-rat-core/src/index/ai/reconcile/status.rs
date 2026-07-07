use super::super::*;

pub(crate) fn status(conn: &Connection) -> anyhow::Result<LlmStatus> {
    ensure_model_manifest(conn)?;
    let total_chunks = chunk_count(conn)?;
    let active_model_id = active_embedding_model_id(conn)?;
    let embedding = capability_status(conn, "embedding", &active_model_id, total_chunks)?;
    let fastembed = fastembed_operational_status(conn, &active_model_id, total_chunks)?;
    let current = embedding.current_artifacts;
    let stale = embedding.stale_artifacts;
    let failed = embedding.failed_artifacts;
    let blocked = embedding.blocked_artifacts;
    let missing = total_chunks.saturating_sub(current + stale + failed + blocked);
    let skipped_chunks = fastembed.skipped_embeddings;
    let eligible_chunks = total_chunks.saturating_sub(skipped_chunks);
    Ok(LlmStatus {
        embedding,
        artifacts: ArtifactCounts {
            total_chunks,
            eligible_chunks,
            skipped_chunks,
            current,
            missing,
            stale,
            failed,
            blocked,
            disabled: 0,
        },
        fastembed,
        last_reconcile: last_reconcile_status(conn)?,
    })
}

/// How many chunks in the ACTIVE scope still need embedding (missing / stale / model- or
/// dim-changed / retryable-failed), using the SAME candidate sizing the reconcile loop uses. The
/// watcher gates a per-overlay reconcile on `this_changed`; a reconcile that returned `Partial`
/// (the shared budget ran out mid-pass) leaves a backlog the next pass would skip because the
/// overlay rows themselves did not change — so the watcher also reconciles an overlay whose `> 0`
/// pending count means embeddings were left behind (#219 review). Returns 0 when no embedding model
/// is ready (nothing to retry) rather than erroring — the watcher must not abort a pass over a
/// missing embedder.
pub(crate) fn pending_embedding_jobs(conn: &Connection) -> anyhow::Result<u64> {
    pending_embedding_jobs_with_options(conn, &ReconcileOptions::default())
}

pub(crate) fn pending_embedding_jobs_with_options(
    conn: &Connection,
    options: &ReconcileOptions,
) -> anyhow::Result<u64> {
    let Some((model_id, model_version, dim, max_embedding_chars)) =
        ready_embedding_scan_parts(conn, options)?
    else {
        return Ok(0);
    };
    let scan = EmbeddingScan {
        model_id: &model_id,
        model_version: &model_version,
        dim,
        max_embedding_chars,
    };
    estimated_reconcile_jobs(conn, &scan, options)
}

/// Count pending work for watcher backlog retries, but first prove an ephemeral incremental pass
/// can actually embed. Without this guard, an absent/down local `query_endpoint` pays the O(repo)
/// candidate scan on every startup only to have reconcile immediately return `SkipEphemeral`.
pub(crate) fn pending_embedding_jobs_with_available_incremental_embedder(
    conn: &Connection,
    options: &ReconcileOptions,
) -> anyhow::Result<u64> {
    let Some(remote) = active_remote_config(conn)? else {
        return pending_embedding_jobs_with_options(conn, options);
    };
    if !remote.is_ephemeral() {
        return pending_embedding_jobs_with_options(conn, options);
    }
    let Some((model_id, model_version, dim, max_embedding_chars)) =
        ready_embedding_scan_parts(conn, options)?
    else {
        return Ok(0);
    };
    let scan = EmbeddingScan {
        model_id: &model_id,
        model_version: &model_version,
        dim,
        max_embedding_chars,
    };
    let mut light_options = options.clone();
    light_options.provision_remote = false;
    match acquire_chunk_embedder(conn, light_options.intra_threads, &scan, &light_options) {
        ChunkEmbedder::Ready { .. } => estimated_reconcile_jobs(conn, &scan, &light_options),
        ChunkEmbedder::SkipEphemeral
        | ChunkEmbedder::NoEphemeralWork
        | ChunkEmbedder::NotReady(_) => Ok(0),
    }
}

fn ready_embedding_scan_parts(
    conn: &Connection,
    options: &ReconcileOptions,
) -> anyhow::Result<Option<(String, String, usize, usize)>> {
    ensure_model_manifest(conn)?;
    let model_id = active_embedding_model_id(conn)?;
    let model = model(conn, &model_id)?;
    if validate_ready_model(&model).is_err() {
        return Ok(None);
    }
    let model_version = active_embedding_model_version(conn, &model_id)?;
    let dim = usize::try_from(model.embedding_dim.unwrap_or_default()).unwrap_or(0);
    let max_embedding_chars = options.max_embedding_chars.max(MIN_EMBEDDING_CHARS);
    Ok(Some((model_id, model_version, dim, max_embedding_chars)))
}

pub(crate) fn reconcile_plan(conn: &Connection) -> anyhow::Result<ReconcilePlan> {
    ensure_model_manifest(conn)?;
    let model_id = active_embedding_model_id(conn)?;
    let model = model(conn, &model_id)?;
    let model_version = active_embedding_model_version(conn, &model_id)?;
    let dim = usize::try_from(model.embedding_dim.unwrap_or_default()).unwrap_or(0);
    let available = validate_ready_model(&model).is_ok();
    let message = (!available).then(|| model_not_ready_reason(&model));
    Ok(ReconcilePlan {
        embeddings: embedding_reconcile_plan(
            conn,
            &model,
            &model_version,
            dim,
            available,
            message,
        )?,
        summaries: SummaryReconcilePlan {
            enabled: false,
            message: "summaries are not implemented yet".to_string(),
        },
    })
}

pub(crate) fn embedding_reconcile_plan(
    conn: &Connection,
    model: &ModelInfo,
    model_version: &str,
    dim: usize,
    available: bool,
    message: Option<String>,
) -> anyhow::Result<EmbeddingReconcilePlan> {
    let skipped_by_policy = embedding_policy_skip_summary(conn, DEFAULT_MAX_EMBEDDING_CHARS)?;
    let mut missing_by_priority = BTreeMap::new();
    let mut current = 0_u64;
    let mut missing = 0_u64;
    let mut stale = 0_u64;
    let mut model_changed = 0_u64;
    let mut dim_changed = 0_u64;
    let mut failed_retryable = 0_u64;
    let mut failed_waiting = 0_u64;
    let mut blocked = 0_u64;
    // STREAM the candidates (need-first) and count — never materialize every candidate's
    // decompressed text at once (#379). Same per-job classification as the old `for job in jobs`
    // over `embedding_job_candidates(None)`, so the counts are identical; only the peak memory
    // drops.
    for_each_embedding_candidate(conn, &model.model_id, model_version, dim, None, false, |job| {
        let policy = policy_for_job(&job, DEFAULT_MAX_EMBEDDING_CHARS);
        if !policy.eligible {
            return Ok(());
        }
        let current_artifact = job.embedding_status.as_deref() == Some("Current")
            && job.source_text_hash.as_deref() == Some(job.text_hash.as_str())
            && job.model_version.as_deref() == Some(model_version)
            && job.embedding_dim == Some(i64::try_from(dim).unwrap_or(i64::MAX))
            && job.embedding_text_version.as_deref() == Some(EMBEDDING_TEXT_VERSION)
            && job.input_hash.as_deref().is_some_and(|input_hash| {
                let input = build_embedding_input(&job, DEFAULT_MAX_EMBEDDING_CHARS);
                input_hash == embedding_input_hash(&model.model_id, model_version, &input.text)
            });
        if current_artifact {
            current += 1;
            return Ok(());
        }
        let reason = job.reason(model_version, dim, now_ms(), DEFAULT_MAX_EMBEDDING_CHARS);
        match reason {
            ReconcileReason::Missing => missing += 1,
            ReconcileReason::SourceChanged => stale += 1,
            ReconcileReason::InputChanged => stale += 1,
            ReconcileReason::ModelChanged => model_changed += 1,
            ReconcileReason::DimChanged => dim_changed += 1,
            ReconcileReason::RetryAfterFailure => failed_retryable += 1,
            ReconcileReason::Forced => missing += 1,
        }
        *missing_by_priority.entry(priority_label(policy.priority).to_string()).or_default() += 1;
        if job.embedding_status.as_deref() == Some("Failed")
            && job.next_retry_after_ms.unwrap_or(0) > now_ms()
        {
            failed_waiting += 1;
        }
        if job.embedding_status.as_deref() == Some("Blocked") {
            blocked += 1;
        }
        Ok(())
    })?;
    Ok(EmbeddingReconcilePlan {
        model_id: model.model_id.clone(),
        model_version: model_version.to_string(),
        dim,
        available,
        current,
        missing,
        stale,
        model_changed,
        dim_changed,
        failed_retryable,
        failed_waiting,
        blocked,
        disabled: u64::from(model.disabled),
        skipped_total: skipped_by_policy.values().sum(),
        skipped_by_policy,
        missing_by_priority,
        message,
    })
}

pub(crate) fn last_reconcile_status(
    conn: &Connection,
) -> anyhow::Result<Option<LastReconcileStatus>> {
    // Scoped to the active repo (V042): `reconcile_attempts` is a global append-only log, so the
    // "latest attempt" pick must filter `repo_id` — else a sibling repo's newer attempt on a
    // consolidated DB would be reported as this repo's status. `{repo_clause}` empty pre-A5.
    let scope = crate::index::schema::periphery_repo_scope(conn, "reconcile_attempts")?;
    let repo_clause =
        crate::index::schema::periphery_repo_scope_clause(&scope, "reconcile_attempts");
    conn.query_row(
        &format!(
            "
        SELECT started_at_ms,
               finished_at_ms,
               batch_size,
               processed_chunks,
               embeddings_written,
               blocked_chunks,
               elapsed_ms,
               input_chars,
               status,
               message
        FROM reconcile_attempts
        WHERE 1=1{repo_clause}
        ORDER BY started_at_ms DESC, id DESC
        LIMIT 1
        "
        ),
        [],
        |row| {
            let elapsed_ms = u64::try_from(row.get::<_, i64>(6)?).unwrap_or(0);
            let input_chars = u64::try_from(row.get::<_, i64>(7)?).unwrap_or(0);
            let embeddings_written = u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0);
            let elapsed_secs = (elapsed_ms as f64 / 1000.0).max(0.001);
            Ok(LastReconcileStatus {
                started_at_ms: row.get(0)?,
                finished_at_ms: row.get(1)?,
                batch_size: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                processed_chunks: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                embeddings_written,
                blocked_chunks: u64::try_from(row.get::<_, i64>(5)?).unwrap_or(0),
                elapsed_ms,
                input_chars,
                chunks_per_sec: embeddings_written as f64 / elapsed_secs,
                chars_per_sec: input_chars as f64 / elapsed_secs,
                status: row.get(8)?,
                message: row.get(9)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}
