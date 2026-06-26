use super::*;
use crate::config::RemoteEmbeddingConfig;
use crate::embedding_models::{
    Backend, EmbeddingModelSpec, FASTEMBED_DISPLAY_MODEL, FASTEMBED_EMBEDDING_DIM,
    FASTEMBED_MODEL_ID, spec,
};

pub(crate) fn install_fastembed_model(conn: &Connection, model_id: &str) -> anyhow::Result<()> {
    #[cfg(feature = "fastembed")]
    {
        // Init the model that matches `model_id` (triggers its download + yields the right dim) —
        // each fastembed model pulls its own weights and reports its own dim (#112). The dim comes
        // from the registry spec; jina-v2-code is 768-dim, not 384.
        let spec = spec(model_id)
            .ok_or_else(|| anyhow::anyhow!("unknown fastembed model `{model_id}`"))?;
        let embedder = FastEmbedEmbedder::for_model_id(spec.model_id, spec.dim, None)
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

/// Install (activate) the SELECTED model served over Ollama (#317 rework). Unlike the local
/// backends there is NO download: the "install" is a reachability + dim-parity PROBE. We construct
/// the real [`OllamaEmbedder`] for the selected model and embed a single `"ping"` — that one call
/// validates the endpoint is reachable, auth resolves, AND the server's vector width matches the
/// selected model's dim (the embedder's per-batch dim contract), reusing the exact connection the
/// reconcile loop will use. A probe failure REFUSES the install loudly (the row is left
/// not-installed); we do not write a half-ready model that would then fail every reconcile batch.
///
/// On success: toggle the SAME `ai_models` row to `runtime='ollama'`, Ready, and persist the remote
/// config to the secret-free meta so `active_embedder` can reconstruct the embedder. The freshness
/// version is NOT written here — it is stamped centrally by the caller (`install_model`) for every
/// runtime (see [`remote_freshness_version`] + the `install_model` single-writer comment).
pub(crate) fn install_ollama_model(
    conn: &Connection,
    model_id: &str,
    spec: &EmbeddingModelSpec,
    remote: &RemoteEmbeddingConfig,
) -> anyhow::Result<()> {
    // Probe: construct + one-shot embed. Reachability, auth, AND the dim contract are validated in
    // this single call (the embedder checks every returned vector against the selected model's
    // dim).
    let embedder =
        OllamaEmbedder::from_remote_config(remote, spec.model_id, spec.dim).map_err(|err| {
            anyhow::anyhow!("failed to construct ollama embedder for `{model_id}`: {err}")
        })?;
    embedder.embed_batch(&["ping".to_string()]).map_err(|err| {
        anyhow::anyhow!(
            "ollama reachability/dim probe failed for `{model_id}` (endpoint `{}`, model `{}`): \
             {err}",
            remote.endpoint.as_deref().unwrap_or("<none>"),
            remote.model
        )
    })?;
    conn.execute(
        "UPDATE ai_models
         SET installed = 1, disabled = 0, status = 'Ready', installed_at_ms = ?2,
             embedding_dim = ?3, runtime = 'ollama', last_error = NULL
         WHERE model_id = ?1",
        params![model_id, now_ms(), i64::try_from(spec.dim).unwrap_or(i64::MAX)],
    )?;
    set_active_remote_config(conn, remote)?;
    Ok(())
}

/// The reconcile freshness key for the SELECTED model when served over Ollama. Distinct from the
/// static `spec.version` (the local key) so a local↔remote flip re-embeds, and folds in the
/// server-side `remote.model` so switching THAT re-embeds too.
///
/// DELIBERATELY ENDPOINT-INDEPENDENT (#317 rework — the key fix). The endpoint does NOT define the
/// vector space; the model + runtime do. Folding the endpoint host would re-embed the WHOLE repo on
/// every run for ephemeral/cookbook boxes (each gets a fresh URL). So: new URL + same
/// `remote.model` → SAME freshness → no re-embed; a different `remote.model`, or a local↔remote
/// flip → re-embed.
pub(crate) fn remote_freshness_version(
    spec: &EmbeddingModelSpec,
    remote: &RemoteEmbeddingConfig,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(spec.version.as_bytes());
    hasher.update([0]);
    // The "ollama" runtime marker is what distinguishes this remote key from the local
    // `spec.version` even when `remote.model` happens to be empty — a local↔remote flip always
    // re-embeds.
    hasher.update(Backend::Ollama.runtime().as_bytes());
    hasher.update([0]);
    hasher.update(remote.model.trim().as_bytes());
    let digest = hasher.finalize();
    // 8 hex bytes (16 chars) is ample to separate model/runtime combinations; keep the
    // human-legible `spec.version` prefix so the persisted value still reads as this model.
    let short: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{}-{short}", spec.version)
}

pub(crate) fn fastembed_operational_status(
    conn: &Connection,
    active_model_id: &str,
    total_chunks: u64,
) -> anyhow::Result<FastEmbedOperationalStatus> {
    // Report the ACTIVE fastembed model (all-MiniLM / BGE-small / jina-code), not always all-MiniLM
    // — else installing another fastembed model makes status/doctor flag MiniLM as missing or
    // needing reconcile (#112 review). A model is "a fastembed model" iff its registry backend is
    // FastEmbed; everything non-fastembed falls back to the MiniLM report identity.
    let active_is_fastembed = spec(active_model_id).map(|s| s.backend) == Some(Backend::FastEmbed);
    let report_model_id = if active_is_fastembed { active_model_id } else { FASTEMBED_MODEL_ID };
    let report_display = spec(report_model_id).map_or(FASTEMBED_DISPLAY_MODEL, |s| s.display);
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
        // The reported dim must reflect the ACTIVE model (jina-code is 768, not 384) — driven off
        // the registry spec so it never regresses to the MiniLM default for a 768-dim active model.
        dim: spec(report_model_id).map_or(FASTEMBED_EMBEDDING_DIM, |s| s.dim),
        cache: fastembed_cache_dir().display().to_string(),
        installed: model.installed,
        active: active_is_fastembed,
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

#[cfg(test)]
mod ollama_install_tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;
    use crate::config::RemoteMode;
    use crate::embedding_models::FASTEMBED_MODEL_ID;

    /// One-shot HTTP/1.1 stub on an ephemeral port that replies to the probe's single `/api/embed`
    /// POST with `{"embeddings":[[<dim floats>]]}`. Returns the base URL + the server join handle.
    fn spawn_embed_stub(dim: usize) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let nums = vec!["0.1"; dim].join(",");
                let body = format!("{{\"embeddings\":[[{nums}]]}}");
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

    fn schema_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        ensure_model_manifest(&conn).unwrap();
        conn
    }

    fn remote_at(endpoint: &str) -> RemoteEmbeddingConfig {
        RemoteEmbeddingConfig {
            mode: RemoteMode::Connect,
            model: "all-minilm".to_string(),
            endpoint: Some(endpoint.to_string()),
            auth_env: None,
            batch_size: 256,
            request_timeout_s: 5,
        }
    }

    #[test]
    fn install_ollama_model_toggles_the_selected_models_row_to_ollama_runtime() {
        // #317 rework: serving a real model (fastembed all-minilm) over Ollama toggles ITS row's
        // runtime to `ollama` — the same model_id + dim, transport overridden.
        let selected = spec(FASTEMBED_MODEL_ID).unwrap();
        let (url, handle) = spawn_embed_stub(selected.dim);
        let conn = schema_conn();
        let remote = remote_at(&url);

        install_ollama_model(&conn, FASTEMBED_MODEL_ID, selected, &remote).expect("probe succeeds");
        handle.join().unwrap();

        let model = model(&conn, FASTEMBED_MODEL_ID).unwrap();
        assert!(model.installed);
        assert_eq!(model.status, "Ready");
        assert_eq!(model.runtime, "ollama", "the row's runtime toggled to ollama");
        assert_eq!(model.embedding_dim, Some(selected.dim as i64));
        assert_eq!(model.last_error, None);

        // The remote config is persisted so active_embedder can reconstruct the embedder.
        assert_eq!(active_remote_config(&conn).unwrap(), Some(remote.clone()));

        // NOTE: the freshness version meta is NOT written here — it is stamped centrally by
        // `install_model` for every runtime (covered by reconcile.rs `freshness_version_tests`).
    }

    #[test]
    fn install_ollama_model_refuses_on_a_dim_mismatch() {
        // The server returns a 512-dim vector; the selected model is 384 — the probe's per-batch
        // dim contract rejects it, so the install must refuse and leave the row not
        // flipped.
        let selected = spec(FASTEMBED_MODEL_ID).unwrap();
        let (url, handle) = spawn_embed_stub(512);
        let conn = schema_conn();

        let err = install_ollama_model(&conn, FASTEMBED_MODEL_ID, selected, &remote_at(&url))
            .expect_err("dim mismatch must refuse the install");
        handle.join().unwrap();
        assert!(err.to_string().contains("384") || err.to_string().contains("512"), "{err}");

        // No remote config was persisted.
        assert_eq!(active_remote_config(&conn).unwrap(), None);
    }

    #[test]
    fn remote_freshness_version_is_endpoint_independent() {
        // THE KEY FIX (#317 rework): two DIFFERENT endpoints with the SAME server-side model must
        // yield the SAME freshness version — an ephemeral box's per-run URL must NOT re-embed the
        // whole repo. The endpoint does not define the vector space; the model + runtime do.
        let spec = spec(FASTEMBED_MODEL_ID).unwrap();
        let a = remote_freshness_version(spec, &remote_at("http://a.example:11434"));
        let b = remote_freshness_version(spec, &remote_at("http://b.example:11434"));
        assert_eq!(a, b, "different endpoints, same model → SAME freshness (no re-embed)");
        assert_ne!(
            a, spec.version,
            "remote key differs from the local static version (flip re-embeds)"
        );
        assert!(a.starts_with(spec.version), "keeps the legible model prefix: {a}");
    }

    #[test]
    fn remote_freshness_version_differs_by_server_model() {
        // Switching the SERVER-side model DOES bump the key (a different vector space).
        let spec = spec(FASTEMBED_MODEL_ID).unwrap();
        let mut a = remote_at("http://localhost:11434");
        let mut b = a.clone();
        a.model = "all-minilm".to_string();
        b.model = "nomic-embed-text".to_string();
        assert_ne!(
            remote_freshness_version(spec, &a),
            remote_freshness_version(spec, &b),
            "switching the server-side model must bump the freshness version",
        );
    }
}
