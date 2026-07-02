use super::super::*;
use crate::config::RemoteEmbeddingConfig;
use crate::embedding_models::{Backend, spec};
// `FASTEMBED_MODEL_ID` is only referenced by the fastembed-gated cache recovery (and tests);
// the manifest's stale-`'v1'` check now scopes to `HASH_MODEL_ID` only (the old fastembed id
// was renamed to an HF path in #317 and is legacy-cleaned), so the prod import is
// feature-gated.
#[cfg(feature = "fastembed")]
use crate::embedding_models::{FASTEMBED_EMBEDDING_DIM, FASTEMBED_MODEL_ID};

pub(crate) fn recover_cached_fastembed_model(conn: &Connection) -> anyhow::Result<()> {
    recover_cached_fastembed_model_from(conn, &fastembed_cache_dir())
}

pub(crate) fn recover_cached_fastembed_model_from(
    conn: &Connection,
    cache_dir: &Path,
) -> anyhow::Result<()> {
    #[cfg(feature = "fastembed")]
    {
        recover_cached_fastembed_model_at(conn, cache_dir)?;
    }
    #[cfg(not(feature = "fastembed"))]
    {
        let _ = (conn, cache_dir);
    }
    Ok(())
}

#[cfg(feature = "fastembed")]
pub(crate) fn recover_cached_fastembed_model_at(
    conn: &Connection,
    cache_dir: &Path,
) -> anyhow::Result<()> {
    if !fastembed_cache_ready(cache_dir) {
        return Ok(());
    }
    let fastembed = model(conn, FASTEMBED_MODEL_ID)?;
    if !fastembed.installed || fastembed.status != "Ready" {
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', installed_at_ms = ?2,
                 embedding_dim = ?3, runtime = 'fastembed', last_error = NULL
             WHERE model_id = ?1",
            params![
                FASTEMBED_MODEL_ID,
                now_ms(),
                i64::try_from(FASTEMBED_EMBEDDING_DIM).unwrap_or(i64::MAX)
            ],
        )?;
    }
    if active_embedding_model_is_missing(conn)? {
        // Activate the recovered model AND stamp its freshness version in ONE call (R3b): a bare
        // `set_meta(ACTIVE_EMBEDDING_MODEL_META, ...)` without the version would leave
        // `active_embedding_model_version` returning a STALE legacy key, so new embeddings bake
        // under the wrong `model_version` and a later install flips it → a spurious full
        // re-embed.
        let spec = spec(FASTEMBED_MODEL_ID)
            .ok_or_else(|| anyhow::anyhow!("unknown model `{FASTEMBED_MODEL_ID}`"))?;
        activate_model_with_version(conn, FASTEMBED_MODEL_ID, spec.version)?;
    }
    Ok(())
}

#[cfg(feature = "fastembed")]
pub(crate) fn active_embedding_model_is_missing(conn: &Connection) -> anyhow::Result<bool> {
    let Some(active_model_id) = meta(conn, ACTIVE_EMBEDDING_MODEL_META)? else {
        return Ok(true);
    };
    let active = conn
        .query_row(
            "
            SELECT model_id, capability, embedding_dim, runtime, installed, disabled, status, \
             installed_at_ms, last_error
            FROM ai_models WHERE model_id = ?1
            ",
            [active_model_id],
            model_row,
        )
        .optional()?;
    Ok(match active {
        Some(active) => validate_ready_model(&active).is_err(),
        None => true,
    })
}

#[cfg(feature = "fastembed")]
pub(crate) fn fastembed_cache_ready(cache_dir: &Path) -> bool {
    let repo = cache_dir.join(FASTEMBED_HF_CACHE_REPO_DIR);
    let Ok(revision) = std::fs::read_to_string(repo.join("refs").join("main")) else {
        return false;
    };
    let revision = revision.trim();
    !revision.is_empty() && repo.join("snapshots").join(revision).is_dir()
}

/// Activate `model_id` as the active embedding model AND stamp its freshness `version` in ONE call
/// — the SINGLE place that writes both metas, so no activation site can set the active model
/// without its version (the bug R3b fixed in recovery). `install_model` and
/// `recover_cached_fastembed_model` both go through here.
pub(crate) fn activate_model_with_version(
    conn: &Connection,
    model_id: &str,
    version: &str,
) -> anyhow::Result<()> {
    set_meta(conn, ACTIVE_EMBEDDING_MODEL_META, model_id)?;
    set_reconcile_meta(conn, ACTIVE_EMBEDDING_MODEL_VERSION_META, version)?;
    Ok(())
}

pub(crate) fn install_model(
    conn: &Connection,
    model_id: &str,
    remote: Option<&RemoteEmbeddingConfig>,
) -> anyhow::Result<ModelInfo> {
    ensure_model_manifest(conn)?;
    // The arg is the model_id — the HF path (no aliases, #317). Resolve to its spec and use the
    // canonical id downstream.
    let spec =
        spec(model_id).ok_or_else(|| anyhow::anyhow!("unknown embedding model `{model_id}`"))?;
    let model_id = spec.model_id;
    // The PRESENCE of a remote block — NOT `spec.backend` — selects the Ollama transport (#317
    // rework): any transformer model can be served over Ollama. The SAME ai_models row toggles its
    // `runtime` local↔ollama. Remote present → reachability + dim probe (no download); else → the
    // local install for the model's backend.
    if let Some(remote) = remote {
        // A remote block serves the model over Ollama, which can only serve TRANSFORMER models. The
        // CLI passes the config's remote block for WHATEVER model id the user typed, so guard HERE
        // too — the config-layer guard only checks `config.model`, not an explicit
        // `models install <other-id>`. Without this, `models install embedding-hash` against a
        // 384-dim Ollama would mark the hash row `runtime='ollama'` and store the served model's
        // vectors under the hash id.
        if spec.backend != Backend::FastEmbed {
            anyhow::bail!(
                "remote embedding requires a transformer model, but `{model_id}` is a {} model — \
                 remove the [llm.embedding.remote] block to install it locally, or select a \
                 transformer model to serve over the remote backend",
                spec.backend.runtime()
            );
        }
        install_remote_model(conn, model_id, spec, remote)?;
    } else {
        match spec.backend {
            Backend::Hash => {
                conn.execute(
                    "UPDATE ai_models
                     SET installed = 1, disabled = 0, status = 'Ready', installed_at_ms = ?2,
                         embedding_dim = ?3, runtime = 'hash', last_error = NULL
                     WHERE model_id = ?1",
                    params![model_id, now_ms(), i64::try_from(spec.dim).unwrap_or(i64::MAX)],
                )?;
            },
            Backend::FastEmbed => install_fastembed_model(conn, model_id)?,
            Backend::Model2Vec => install_model2vec_model(conn, model_id)?,
            // Transport-only runtime — never a local install target (the remote branch above owns
            // the Ollama path).
            Backend::Ollama => anyhow::bail!(
                "internal error: Backend::Ollama is a transport, not a local install target"
            ),
        }
        // A LOCAL install must drop any remote-config meta a PRIOR Ollama install of this model
        // left behind — otherwise `active_embedder` reads it back unconditionally and keeps
        // building an OpenAiEmbedder against the now-removed endpoint instead of the local
        // model.
        clear_active_remote_config(conn)?;
    }
    // Activate + stamp the freshness version in one call (the single writer). The version must
    // reflect the runtime actually installed: a remote install uses the endpoint-INDEPENDENT remote
    // key (so an ephemeral box's new URL with the same `remote.model` does NOT re-embed), and a
    // local install uses the static `spec.version` (so flipping remote→local re-embeds).
    let freshness = match remote {
        Some(remote) => remote_freshness_version(spec, remote),
        None => spec.version.to_string(),
    };
    activate_model_with_version(conn, model_id, &freshness)?;
    model(conn, model_id)
}

pub(crate) fn models(conn: &Connection) -> anyhow::Result<Vec<ModelInfo>> {
    ensure_model_manifest(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT model_id, capability, embedding_dim, runtime, installed, disabled, status, \
         installed_at_ms, last_error
        FROM ai_models
        ORDER BY capability, model_id
        ",
    )?;
    let rows = stmt.query_map([], model_row)?;
    collect_rows(rows)
}

pub(crate) fn install_model2vec_model(conn: &Connection, model_id: &str) -> anyhow::Result<()> {
    #[cfg(feature = "model2vec")]
    {
        let embedder = Model2VecEmbedder::new()?;
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', installed_at_ms = ?2,
                 embedding_dim = ?3, runtime = 'model2vec', last_error = NULL
             WHERE model_id = ?1",
            params![model_id, now_ms(), i64::try_from(embedder.dim()).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }
    #[cfg(not(feature = "model2vec"))]
    {
        conn.execute(
            "UPDATE ai_models
             SET installed = 0, disabled = 0, status = 'MissingRuntime', last_error = ?2
             WHERE model_id = ?1",
            params![model_id, MODEL2VEC_MISSING_FEATURE_MESSAGE],
        )?;
        anyhow::bail!("{}", MODEL2VEC_MISSING_FEATURE_MESSAGE)
    }
}
