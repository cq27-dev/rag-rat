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
        // PROVISIONAL: a cache recovery is automatic, not a user choice — a differing config seed
        // must still win over it on a fresh index (#394 review).
        activate_model_with_version(conn, FASTEMBED_MODEL_ID, spec.version, true)?;
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

/// Activate `model_id` as the active embedding model, stamp its freshness `version`, and record its
/// PROVENANCE — all in ONE call, the SINGLE place that writes these metas, so no activation site
/// can set the active model without its version (the bug R3b fixed in recovery) or its provenance.
/// `provisional = true` for an AUTOMATIC activation (config seed, fastembed-cache recovery),
/// `false` for an EXPLICIT one (`install_model`). See [`ACTIVE_EMBEDDING_MODEL_PROVISIONAL_META`].
pub(crate) fn activate_model_with_version(
    conn: &Connection,
    model_id: &str,
    version: &str,
    provisional: bool,
) -> anyhow::Result<()> {
    set_meta(conn, ACTIVE_EMBEDDING_MODEL_META, model_id)?;
    set_reconcile_meta(conn, ACTIVE_EMBEDDING_MODEL_VERSION_META, version)?;
    set_meta(conn, ACTIVE_EMBEDDING_MODEL_PROVISIONAL_META, if provisional { "1" } else { "0" })?;
    Ok(())
}

/// Whether the active embedding model is PROVISIONAL — set automatically (seed / cache recovery)
/// and not yet confirmed by an explicit install or a committed reconcile. Absent ⇒ non-provisional.
pub(crate) fn active_embedding_model_is_provisional(conn: &Connection) -> anyhow::Result<bool> {
    Ok(meta(conn, ACTIVE_EMBEDDING_MODEL_PROVISIONAL_META)?.as_deref() == Some("1"))
}

/// Clear the provisional flag — the active model is now the CONFIRMED choice (an explicit install,
/// or a reconcile that committed embeddings under it). Idempotent.
pub(crate) fn clear_active_embedding_model_provisional(conn: &Connection) -> anyhow::Result<()> {
    set_meta(conn, ACTIVE_EMBEDDING_MODEL_PROVISIONAL_META, "0")
}

/// Clear the active embedding model entirely — the inverse of [`activate_model_with_version`].
/// Removes the model id, its freshness version, the provenance flag, AND any remote-config meta, so
/// `active_embedding_model_id` returns to the hash fallback and no stale version / endpoint lingers
/// to mislead a later activation. Called when the config disables embeddings (`model = "none"`)
/// while a PROVISIONAL model was active: the embeddings-off choice must supersede an automatic
/// activation, restoring the "embeddings-off ⇒ active model unset" invariant (#394 review).
/// Idempotent (each delete is a no-op when its key is absent).
pub(crate) fn clear_active_embedding_model(conn: &Connection) -> anyhow::Result<()> {
    delete_meta(conn, ACTIVE_EMBEDDING_MODEL_META)?;
    clear_reconcile_meta(conn, ACTIVE_EMBEDDING_MODEL_VERSION_META)?;
    delete_meta(conn, ACTIVE_EMBEDDING_MODEL_PROVISIONAL_META)?;
    clear_active_remote_config(conn)
}

/// Seed the active embedding model from the CONFIG's selection when the index has none yet (#394),
/// so a fresh index adopts the configured model instead of silently defaulting to the hash fallback
/// (`active_embedding_model_id`'s `HASH_MODEL_ID` default — which the config never selects, since
/// `EmbeddingBackend::default()` is all-MiniLM). Read-first via
/// [`active_embedding_model_seed_owed`]: a no-op once the active model is EXPLICIT (an
/// `install_model`) or CONFIRMED (a reconcile committed embeddings under it) — that model is
/// respected. While the active model is PROVISIONAL (a prior seed or a fastembed-cache recovery)
/// config stays authoritative — a config-model edit is adopted, and the embeddings-off case below
/// clears it.
///
/// This does NOT install the model — reconcile still blocks with an accurate
/// `models install <configured>` hint until it is — it only makes the index's active model reflect
/// the config so that hint (and every model-scoped read) names the RIGHT model rather than the hash
/// fallback. Runs on the write-bearing `open_config`, so any write-bearing open heals an index
/// built before this fix.
///
/// The embeddings-off choice (`configured_model_id == None`) is symmetric: it CLEARS a still-active
/// PROVISIONAL model via [`clear_active_embedding_model`] (an explicit / confirmed install is
/// preserved), so switching the config to `model = "none"` stops reconcile / status naming a model
/// the config no longer wants — restoring the "embeddings-off ⇒ active model unset" invariant.
pub(crate) fn seed_active_embedding_model(
    conn: &Connection,
    configured_model_id: Option<&str>,
) -> anyhow::Result<()> {
    if !active_embedding_model_seed_owed(conn, configured_model_id)? {
        return Ok(());
    }
    let Some(model_id) = configured_model_id else {
        // Embeddings-off, and `seed_owed` already confirmed a PROVISIONAL model is still active:
        // clear it so the meta stops naming a model the config disabled.
        return clear_active_embedding_model(conn);
    };
    let Some(spec) = spec(model_id) else {
        return Ok(()); // unknown id (defensive) — leave unset so the hash fallback stands
    };
    // Drop any remote-config meta a prior remote install left behind: the seed activates the model
    // as a LOCAL placeholder, so without this `active_embedder` would build an OpenAiEmbedder for
    // the new active model against the STALE endpoint / server-side model — vectors in the
    // wrong embedding space (#394 review). `install_model`'s local branch clears it the same
    // way.
    clear_active_remote_config(conn)?;
    // Activate as PROVISIONAL (an automatic config seed, not a user choice) + stamp the freshness
    // version through the single writer (never the active meta alone, per the R3b footgun). The
    // static `spec.version` is correct pre-install; `install_model` re-stamps the runtime-accurate
    // version and clears the provisional flag.
    activate_model_with_version(conn, spec.model_id, spec.version, true)
}

/// Read-only test of whether [`seed_active_embedding_model`] would WRITE. The read-only open
/// consults it to fall back to the write path so the seed heals once (same posture as the
/// model-manifest / generated-flags gates). A seed is owed when the config selects a model AND
/// either (a) no active model is set yet, or (b) the active model DIFFERS from config AND is
/// PROVISIONAL — set automatically by a prior seed or a fastembed-cache recovery, NOT by an
/// explicit `models install` and NOT confirmed by a reconcile that committed embeddings under it.
/// An absent provenance flag (a pre-#394 index, or a committed / explicit model) reads as
/// non-provisional → respected. Provenance is an O(1) meta read — no per-open embedding scan, and a
/// RECOVERED cache no longer masquerades as an explicit install (#394 review).
///
/// The embeddings-off choice (`None`) owes a write only to CLEAR a still-active PROVISIONAL model
/// (an explicit / confirmed install is preserved; an already-unset active model needs nothing) —
/// symmetric with the model-selected case, where a provisional active always yields to config.
pub(crate) fn active_embedding_model_seed_owed(
    conn: &Connection,
    configured_model_id: Option<&str>,
) -> anyhow::Result<bool> {
    let Some(configured) = configured_model_id else {
        // Embeddings-off: a write is owed only when a PROVISIONAL model is still active and must be
        // cleared. An unset active (nothing to clear) or an explicit / confirmed one (preserved)
        // owes nothing.
        return Ok(meta(conn, ACTIVE_EMBEDDING_MODEL_META)?.is_some()
            && active_embedding_model_is_provisional(conn)?);
    };
    match meta(conn, ACTIVE_EMBEDDING_MODEL_META)? {
        None => Ok(true),
        Some(active) if active == configured => Ok(false),
        Some(_) => active_embedding_model_is_provisional(conn),
    }
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
    // EXPLICIT: `models install` is a user choice — not provisional, so the config seed never
    // overrides it (#394 review).
    activate_model_with_version(conn, model_id, &freshness, false)?;
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

#[cfg(test)]
mod seed_active_embedding_model_tests {
    use super::*;

    const JINA: &str = "jinaai/jina-embeddings-v2-base-code";
    const MINILM: &str = "sentence-transformers/all-MiniLM-L6-v2";
    const HASH: &str = "embedding-hash";

    /// A schema-applied, manifest-seeded IN-MEMORY connection — no temp dir, so parallel tests
    /// never collide on a shared path (#394 review).
    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        ensure_model_manifest(&conn).unwrap();
        conn
    }

    #[test]
    fn seeds_the_configured_model_when_active_is_unset() {
        let conn = fresh_conn();
        assert!(
            meta(&conn, ACTIVE_EMBEDDING_MODEL_META).unwrap().is_none(),
            "a fresh index has no active embedding model"
        );
        assert!(
            active_embedding_model_seed_owed(&conn, Some(JINA)).unwrap(),
            "seed owed when unset"
        );

        seed_active_embedding_model(&conn, Some(JINA)).unwrap();

        // The fresh index adopts the CONFIGURED model, NOT the HASH_MODEL_ID fallback.
        assert_eq!(active_embedding_model_id(&conn).unwrap(), JINA);
        assert!(
            !active_embedding_model_seed_owed(&conn, Some(JINA)).unwrap(),
            "no longer owed once seeded"
        );
    }

    #[test]
    fn respects_an_explicitly_installed_model() {
        let conn = fresh_conn();
        // An EXPLICIT install (hash is a no-download local install) is a user choice →
        // NON-provisional provenance, so the config seed must not override it even before
        // any reconcile (#394 review).
        install_model(&conn, HASH, None).unwrap();
        assert_eq!(active_embedding_model_id(&conn).unwrap(), HASH);
        assert!(
            !active_embedding_model_is_provisional(&conn).unwrap(),
            "install is not provisional"
        );

        assert!(!active_embedding_model_seed_owed(&conn, Some(JINA)).unwrap());
        seed_active_embedding_model(&conn, Some(JINA)).unwrap();
        assert_eq!(
            active_embedding_model_id(&conn).unwrap(),
            HASH,
            "an explicitly installed model is respected — the seed never overrides it"
        );
    }

    #[test]
    fn config_wins_over_a_provisionally_recovered_model() {
        let conn = fresh_conn();
        // Simulate a fastembed-cache recovery: activated, but PROVISIONAL (automatic, not a user
        // choice). A `models install` would be non-provisional; this must NOT masquerade as one.
        let minilm = spec(MINILM).expect("registry has all-MiniLM");
        activate_model_with_version(&conn, minilm.model_id, minilm.version, true).unwrap();
        assert!(active_embedding_model_is_provisional(&conn).unwrap());

        assert!(active_embedding_model_seed_owed(&conn, Some(JINA)).unwrap());
        seed_active_embedding_model(&conn, Some(JINA)).unwrap();
        assert_eq!(active_embedding_model_id(&conn).unwrap(), JINA, "config wins over a recovery");
    }

    #[test]
    fn reseeds_a_provisional_model_when_the_config_changes() {
        let conn = fresh_conn();
        // Seed jina — PROVISIONAL (an automatic config seed, not confirmed).
        seed_active_embedding_model(&conn, Some(JINA)).unwrap();
        assert_eq!(active_embedding_model_id(&conn).unwrap(), JINA);
        assert!(active_embedding_model_is_provisional(&conn).unwrap());

        // The config is edited to all-MiniLM BEFORE jina is confirmed → the provisional model
        // yields.
        assert!(active_embedding_model_seed_owed(&conn, Some(MINILM)).unwrap());
        seed_active_embedding_model(&conn, Some(MINILM)).unwrap();
        assert_eq!(active_embedding_model_id(&conn).unwrap(), MINILM);
    }

    #[test]
    fn a_confirmed_model_is_respected_over_a_config_change() {
        let conn = fresh_conn();
        // Seed jina (provisional), then CONFIRM it (as a reconcile committing embeddings would).
        seed_active_embedding_model(&conn, Some(JINA)).unwrap();
        clear_active_embedding_model_provisional(&conn).unwrap();

        // A later config change is now respected — a confirmed model is not reseeded.
        assert!(!active_embedding_model_seed_owed(&conn, Some(MINILM)).unwrap());
        seed_active_embedding_model(&conn, Some(MINILM)).unwrap();
        assert_eq!(active_embedding_model_id(&conn).unwrap(), JINA, "a confirmed model is kept");
    }

    #[test]
    fn embeddings_off_leaves_the_active_model_unset() {
        let conn = fresh_conn();
        assert!(!active_embedding_model_seed_owed(&conn, None).unwrap(), "off ⇒ nothing to seed");
        seed_active_embedding_model(&conn, None).unwrap();
        assert!(
            meta(&conn, ACTIVE_EMBEDDING_MODEL_META).unwrap().is_none(),
            "the embeddings-off choice leaves the active model unset (hash fallback stands)"
        );
    }

    #[test]
    fn embeddings_off_clears_a_provisional_active_model() {
        let conn = fresh_conn();
        // A prior open seeded jina PROVISIONALLY, and a remote-config meta was left behind. The
        // user then edits the config to `model = "none"`.
        seed_active_embedding_model(&conn, Some(JINA)).unwrap();
        set_meta(&conn, ACTIVE_EMBEDDING_REMOTE_CONFIG_META, "{\"stale\":true}").unwrap();
        assert!(active_embedding_model_is_provisional(&conn).unwrap());

        assert!(
            active_embedding_model_seed_owed(&conn, None).unwrap(),
            "clearing a still-active provisional model is owed on the switch to embeddings-off"
        );
        seed_active_embedding_model(&conn, None).unwrap();

        // The active model, its freshness version, the provenance flag, and the stale remote config
        // are all gone — `active_embedding_model_id` returns to the hash fallback.
        assert!(meta(&conn, ACTIVE_EMBEDDING_MODEL_META).unwrap().is_none());
        assert!(reconcile_meta(&conn, ACTIVE_EMBEDDING_MODEL_VERSION_META).unwrap().is_none());
        assert!(meta(&conn, ACTIVE_EMBEDDING_MODEL_PROVISIONAL_META).unwrap().is_none());
        assert!(meta(&conn, ACTIVE_EMBEDDING_REMOTE_CONFIG_META).unwrap().is_none());
        assert_eq!(active_embedding_model_id(&conn).unwrap(), HASH);
        assert!(
            !active_embedding_model_seed_owed(&conn, None).unwrap(),
            "idempotent — nothing left to clear"
        );
    }

    #[test]
    fn embeddings_off_preserves_an_explicit_active_model() {
        let conn = fresh_conn();
        // An EXPLICIT install is a deliberate user choice (NON-provisional). Switching the config
        // to embeddings-off must NOT wipe it — only automatic (provisional) activations
        // yield.
        install_model(&conn, HASH, None).unwrap();
        assert!(!active_embedding_model_is_provisional(&conn).unwrap());

        assert!(
            !active_embedding_model_seed_owed(&conn, None).unwrap(),
            "an explicit install is preserved across a switch to embeddings-off"
        );
        seed_active_embedding_model(&conn, None).unwrap();
        assert_eq!(active_embedding_model_id(&conn).unwrap(), HASH, "explicit model kept");
    }
}
