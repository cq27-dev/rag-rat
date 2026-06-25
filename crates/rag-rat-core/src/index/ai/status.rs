use super::*;

pub(crate) fn install_fastembed_model(conn: &Connection, model_id: &str) -> anyhow::Result<()> {
    #[cfg(feature = "fastembed")]
    {
        // Init the model that matches `model_id` (triggers its download + yields the right dim) —
        // each fastembed model pulls its own weights and reports its own dim (#112). jina-v2-code
        // is 768-dim, not 384.
        let embedder = match model_id {
            BGE_SMALL_MODEL_ID => FastEmbedEmbedder::new_bge_small(None),
            JINA_CODE_MODEL_ID => FastEmbedEmbedder::new_jina_code(None),
            _ => FastEmbedEmbedder::new(None),
        }
        .map_err(|err| anyhow::anyhow!("failed to initialize fastembed model: {err}"))?;
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', installed_at_ms = ?2,
                 embedding_dim = ?3, runtime = 'fastembed', last_error = NULL
             WHERE model_id = ?1",
            params![model_id, now_ms(), i64::try_from(embedder.dim()).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }
    #[cfg(not(feature = "fastembed"))]
    {
        conn.execute(
            "UPDATE ai_models
             SET installed = 0, disabled = 0, status = 'MissingRuntime', last_error = ?2
             WHERE model_id = ?1",
            params![model_id, FASTEMBED_MISSING_FEATURE_MESSAGE],
        )?;
        anyhow::bail!("{}", FASTEMBED_MISSING_FEATURE_MESSAGE)
    }
}

pub(crate) fn fastembed_operational_status(
    conn: &Connection,
    active_model_id: &str,
    total_chunks: u64,
) -> anyhow::Result<FastEmbedOperationalStatus> {
    // Report the ACTIVE fastembed model (all-MiniLM or BGE-small), not always all-MiniLM — else
    // installing BGE makes status/doctor flag MiniLM as missing or needing reconcile (#112 review).
    let report_model_id = if matches!(
        active_model_id,
        FASTEMBED_MODEL_ID | BGE_SMALL_MODEL_ID | JINA_CODE_MODEL_ID
    ) {
        active_model_id
    } else {
        FASTEMBED_MODEL_ID
    };
    let report_display = match report_model_id {
        BGE_SMALL_MODEL_ID => BGE_SMALL_DISPLAY_MODEL,
        JINA_CODE_MODEL_ID => JINA_CODE_DISPLAY_MODEL,
        _ => FASTEMBED_DISPLAY_MODEL,
    };
    let model = model(conn, report_model_id)?;
    // PERF: report coverage from CHEAP persisted counts (the `embedding_artifacts` rows + the chunk
    // total) rather than `embedding_reconcile_plan` — which loads EVERY chunk and rebuilds +
    // re-hashes its embedding input (~200s on a 174k-chunk index, paid with OR without an active
    // embedder). `db.status` runs after every `index`, so that plan made the STATUS dominate
    // indexing on a large repo. The status now reports the last-reconciled persisted state (the
    // same basis it uses for the generic `capability_status` counts); the exact policy-skip +
    // live-drift breakdown is the `reconcile --plan` command's job, where the per-chunk scan
    // belongs.
    let current = current_artifact_count(conn, "embedding", report_model_id)?;
    let stale = stale_artifact_count(conn, "embedding", report_model_id)?;
    let failed = status_artifact_count(conn, "embedding", report_model_id, ArtifactStatus::Failed)?;
    let blocked =
        status_artifact_count(conn, "embedding", report_model_id, ArtifactStatus::Blocked)?;
    // Exact `skipped` (embedding input too large) needs the decompressed text per chunk — deferred
    // to `reconcile --plan`. The status treats every chunk as eligible, so the invariant
    // `eligible + skipped == total` holds with `skipped == 0`.
    let eligible = total_chunks;
    let missing = eligible.saturating_sub(
        current.saturating_add(stale).saturating_add(failed).saturating_add(blocked),
    );
    let next = if !fastembed_build_feature_enabled() {
        Some("cargo install rag-rat".to_string())
    } else if validate_ready_model(&model).is_err() {
        Some(format!("rag-rat models install {report_model_id}"))
    } else if missing > 0 || stale > 0 || failed > 0 {
        Some("rag-rat reconcile --limit 500".to_string())
    } else {
        None
    };
    Ok(FastEmbedOperationalStatus {
        backend: "fastembed".to_string(),
        build_feature_enabled: fastembed_build_feature_enabled(),
        model_id: report_model_id.to_string(),
        model: report_display.to_string(),
        dim: expected_dim(report_model_id).unwrap_or(FASTEMBED_EMBEDDING_DIM),
        cache: fastembed_cache_dir().display().to_string(),
        installed: model.installed,
        active: matches!(
            active_model_id,
            FASTEMBED_MODEL_ID | BGE_SMALL_MODEL_ID | JINA_CODE_MODEL_ID
        ),
        status: model.status,
        current_embeddings: current,
        eligible_embeddings: eligible,
        skipped_embeddings: 0,
        stale_embeddings: stale,
        missing_embeddings: missing,
        failed_embeddings: failed,
        // The retryable/waiting split needs the per-row next-retry timestamp; the status reports
        // the aggregate failed count (treated as retryable) and leaves the precise split to
        // `reconcile --plan`.
        failed_retryable_embeddings: failed,
        failed_waiting_embeddings: 0,
        message: model.last_error,
        next,
    })
}

pub(crate) fn fastembed_build_feature_enabled() -> bool {
    cfg!(feature = "fastembed")
}

pub(crate) fn capability_status(
    conn: &Connection,
    capability: &str,
    model_id: &str,
    total_chunks: u64,
) -> anyhow::Result<CapabilityStatus> {
    let model = model(conn, model_id)?;
    let current = current_artifact_count(conn, capability, model_id)?;
    let stale = stale_artifact_count(conn, capability, model_id)?;
    let failed = status_artifact_count(conn, capability, model_id, ArtifactStatus::Failed)?;
    let blocked = status_artifact_count(conn, capability, model_id, ArtifactStatus::Blocked)?;
    let state = if model.disabled {
        "Disabled"
    } else if total_chunks == 0 {
        "IndexEmpty"
    } else if !model.installed {
        "MissingModel"
    } else if failed > 0 {
        "Failed"
    } else {
        "Ready"
    };
    Ok(CapabilityStatus {
        capability: capability.to_string(),
        model_id: model_id.to_string(),
        state: state.to_string(),
        installed: model.installed,
        disabled: model.disabled,
        current_artifacts: current,
        stale_artifacts: stale,
        failed_artifacts: failed,
        blocked_artifacts: blocked,
        message: model.last_error,
    })
}

pub(crate) fn model(conn: &Connection, model_id: &str) -> anyhow::Result<ModelInfo> {
    Ok(conn.query_row(
        "
        SELECT model_id, capability, embedding_dim, runtime, installed, disabled, status, \
         installed_at_ms, last_error
        FROM ai_models WHERE model_id = ?1
        ",
        [model_id],
        model_row,
    )?)
}

pub(crate) fn model_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModelInfo> {
    Ok(ModelInfo {
        model_id: row.get(0)?,
        capability: row.get(1)?,
        embedding_dim: row.get(2)?,
        runtime: row.get(3)?,
        installed: row.get::<_, bool>(4)?,
        disabled: row.get::<_, bool>(5)?,
        status: row.get(6)?,
        installed_at_ms: row.get(7)?,
        last_error: row.get(8)?,
    })
}
