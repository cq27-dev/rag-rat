use super::super::*;
use crate::embedding_models::{EMBEDDING_MODELS, HASH_MODEL_ID};

pub(crate) fn ensure_model_manifest(conn: &Connection) -> anyhow::Result<()> {
    // Read-first: skip the (write-locking) DML entirely when the manifest already matches what we
    // would write. `ensure_model_manifest` runs on EVERY `IndexDatabase::open*`, so issuing
    // unconditional INSERT/UPDATE/DELETE here made every open — including read-only MCP tools —
    // take the SQLite write lock, serializing them against the watcher and other clients and
    // surfacing "database is locked" under concurrency (#143). After the first open establishes
    // the manifest, every later open is a handful of SELECTs and never touches the write lock.
    if model_manifest_is_current(conn)? {
        return Ok(());
    }
    remove_legacy_models(conn)?;
    // One row per registered model, straight from the registry — adding a model needs no edit here.
    // `installed_by_default` is false for EVERY model (including hash): a model is installed only
    // on explicit `install_model`. `upsert_model` is `ON CONFLICT DO NOTHING`, so this only
    // seeds rows.
    for s in EMBEDDING_MODELS {
        upsert_model(conn, s.model_id, "embedding", Some(s.dim), s.backend.runtime(), false)?;
    }
    normalize_embedding_model_versions(conn)?;
    Ok(())
}

/// Read-only test of whether `ensure_model_manifest` would be a no-op — i.e. the manifest is
/// already in its target state. Mirrors exactly the three writes in `ensure_model_manifest`:
/// no legacy model rows linger, all three current models are present (`upsert_model` is
/// `ON CONFLICT DO NOTHING`, so presence is the only condition), and no `chunk_embeddings` row
/// still carries the pre-normalization `'v1'` model_version. Used both to short-circuit the open
/// write path (#143) and to let the read-only MCP open refuse to serve when a manifest write is
/// still owed (falling back to the read-write open, which heals once).
pub(crate) fn model_manifest_is_current(conn: &Connection) -> anyhow::Result<bool> {
    // The active-model meta moved to `repo_meta` (V039); check the active repo's row there.
    let repo_id = crate::index::schema::single_repo_id(conn)?;
    for model_id in LEGACY_MODEL_IDS {
        let lingering: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ai_models WHERE model_id = ?1)
                 OR EXISTS(SELECT 1 FROM chunk_embeddings WHERE model_id = ?1)
                 OR EXISTS(SELECT 1 FROM repo_meta WHERE repo_id = ?3 AND key = ?2 AND value = ?1)",
            params![model_id, ACTIVE_EMBEDDING_MODEL_META, repo_id],
            |row| row.get(0),
        )?;
        if lingering {
            return Ok(false);
        }
    }
    for s in EMBEDDING_MODELS {
        let present: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ai_models WHERE model_id = ?1)",
            params![s.model_id],
            |row| row.get(0),
        )?;
        if !present {
            return Ok(false);
        }
    }
    // Only `embedding-hash` can still carry the pre-#112 bare `'v1'` model_version: the old
    // fastembed id was renamed to an HF path in #317 and is now legacy (deleted above), and the new
    // HF-path id is fresh. Mirror `normalize_embedding_model_versions`.
    let stale_version: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM chunk_embeddings
             WHERE model_version = 'v1' AND model_id = ?1
         )",
        params![HASH_MODEL_ID],
        |row| row.get(0),
    )?;
    Ok(!stale_version)
}

pub(crate) fn remove_legacy_models(conn: &Connection) -> anyhow::Result<()> {
    // The active-model meta moved to `repo_meta` (V039); clear it for the active repo.
    let repo_id = crate::index::schema::single_repo_id(conn)?;
    for model_id in LEGACY_MODEL_IDS {
        conn.execute("DELETE FROM chunk_embeddings WHERE model_id = ?1", params![model_id])?;
        conn.execute("DELETE FROM ai_models WHERE model_id = ?1", params![model_id])?;
        // If this legacy id was the ACTIVE model, its active-model meta AND any persisted
        // remote-config meta (a legacy `ollama-*` id was a remote install — #317) must both go.
        // Leaving the remote config behind would let `active_embedder` keep reconstructing an
        // `OpenAiEmbedder` against a now-removed endpoint after the active model fell back to hash,
        // so clear it whenever we delete the matching active-model meta.
        let was_active = conn.execute(
            "DELETE FROM repo_meta WHERE repo_id = ?1 AND key = ?2 AND value = ?3",
            params![repo_id, ACTIVE_EMBEDDING_MODEL_META, model_id],
        )?;
        if was_active > 0 {
            clear_active_remote_config(conn)?;
            // ALSO drop the legacy model's freshness-version meta (R3a): otherwise the hash
            // fallback inherits the removed model's `model_version` key and reports the
            // wrong freshness. The next install re-stamps it; clearing here keeps the
            // gap correct.
            delete_repo_meta(conn, ACTIVE_EMBEDDING_MODEL_VERSION_META)?;
        }
    }
    Ok(())
}

pub(crate) fn normalize_embedding_model_versions(conn: &Connection) -> anyhow::Result<()> {
    // One-time fix for the pre-#112 bare `'v1'` model_version. Only `embedding-hash` is still a
    // current id: the old `fastembed-all-minilm-l6-v2` was renamed to an HF path in #317 and is now
    // a LEGACY id (its rows are deleted by `remove_legacy_models`), so it no longer needs
    // normalizing here.
    conn.execute(
        "
        UPDATE chunk_embeddings
        SET model_version = 'hash-v1'
        WHERE model_version = 'v1' AND model_id = 'embedding-hash'
        ",
        [],
    )?;
    Ok(())
}

pub(crate) fn upsert_model(
    conn: &Connection,
    model_id: &str,
    capability: &str,
    embedding_dim: Option<usize>,
    runtime: &str,
    installed_by_default: bool,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, disabled, \
         status, installed_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)
        ON CONFLICT(model_id) DO NOTHING
        ",
        params![
            model_id,
            capability,
            embedding_dim.map(|dim| i64::try_from(dim).unwrap_or(i64::MAX)),
            runtime,
            installed_by_default,
            if installed_by_default { "Ready" } else { "MissingModel" },
            installed_by_default.then(now_ms),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod manifest_idempotence_tests {
    use super::*;
    use crate::storage::IndexConnection;

    // #143: `ensure_model_manifest` runs on every `IndexDatabase::open*`. It must be a no-op (no
    // write lock) once the manifest is current, or every read tool serializes on the SQLite writer.
    #[test]
    fn ensure_model_manifest_does_not_write_when_already_current() {
        let dir = std::env::temp_dir().join(format!("ragrat-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("index.db");

        // First open establishes the manifest (a write); afterward the read-only check sees it.
        {
            let rw = IndexConnection::open(&db).unwrap();
            crate::index::schema::apply(rw.connection()).unwrap();
            assert!(
                !model_manifest_is_current(rw.connection()).unwrap(),
                "a freshly applied schema has no model rows yet"
            );
            ensure_model_manifest(rw.connection()).unwrap();
            assert!(model_manifest_is_current(rw.connection()).unwrap());
        }

        // A current manifest means ensure_model_manifest issues NO DML — prove it by running it on
        // a read-only connection, which would error if any INSERT/UPDATE/DELETE executed.
        {
            let ro = IndexConnection::open_read_only_blocking(&db).unwrap();
            assert!(model_manifest_is_current(ro.connection()).unwrap());
            ensure_model_manifest(ro.connection())
                .expect("a current manifest must not write on a read-only connection");
        }

        // A lingering legacy model row flips the check back to "needs work".
        {
            let rw = IndexConnection::open(&db).unwrap();
            rw.connection()
                .execute(
                    "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, \
                     installed, disabled, status, installed_at_ms) VALUES (?1, 'embedding', 384, \
                     'hash', 0, 0, 'MissingModel', NULL)",
                    params![LEGACY_MODEL_IDS[0]],
                )
                .unwrap();
            assert!(
                !model_manifest_is_current(rw.connection()).unwrap(),
                "a lingering legacy model must require a manifest write"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
