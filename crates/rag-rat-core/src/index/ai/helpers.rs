use rag_rat_base::config::RemoteEmbeddingConfig;
use rag_rat_base::embedding_models::{HASH_MODEL_ID, spec};

use super::*;

pub(crate) fn embed_query(
    conn: &Connection,
    query: &str,
) -> anyhow::Result<Option<QueryEmbedding>> {
    ensure_model_manifest(conn)?;
    let Ok(embedder) = active_embedder(conn, None) else {
        return Ok(None);
    };
    // A RUNTIME embed failure on the QUERY path degrades to lexical (BM25), exactly like an
    // unavailable model — `None` means "no query vector, fall back". This matters for the remote
    // (Ollama) backend: a transient endpoint outage (timeout / connection refused / 5xx) must not
    // make EVERY `search` call error; the user still gets BM25 results. This is the QUERY path ONLY
    // — the reconcile/chunk path keeps propagating the error so chunks are marked failed/retryable
    // and re-embedded once the endpoint recovers.
    match embed_query_with(&*embedder, query) {
        Ok(embedding) => Ok(Some(embedding)),
        Err(err) => {
            eprintln!(
                "rag-rat: query embedding failed ({}), falling back to lexical search: {err}",
                embedder.model_id()
            );
            Ok(None)
        },
    }
}

pub(crate) fn hash_query_embedding(query: &str) -> anyhow::Result<QueryEmbedding> {
    embed_query_with(&HashEmbedder, query)
}

pub(crate) fn embed_query_with(
    embedder: &dyn Embedder,
    query: &str,
) -> anyhow::Result<QueryEmbedding> {
    let texts = vec![query.to_string()];
    let mut vectors = embedder.embed_batch(&texts)?;
    let Some(vector) = vectors.pop() else {
        anyhow::bail!("embedder {} returned no query vector", embedder.model_id());
    };
    if vector.len() != embedder.dim() {
        anyhow::bail!(
            "embedder {} returned query dimension {}, expected {}",
            embedder.model_id(),
            vector.len(),
            embedder.dim()
        );
    }
    Ok(QueryEmbedding { model_id: embedder.model_id().to_string(), dim: embedder.dim(), vector })
}

pub(crate) fn active_embedding_model_id(conn: &Connection) -> anyhow::Result<String> {
    ensure_model_manifest(conn)?;
    if let Some(model_id) = repo_meta(conn, ACTIVE_EMBEDDING_MODEL_META)? {
        return Ok(model_id);
    }
    Ok(HASH_MODEL_ID.to_string())
}

pub(crate) fn active_embedding_model_version(
    conn: &Connection,
    model_id: &str,
) -> anyhow::Result<String> {
    // The `embedding_active_model_version` meta is the freshness key of the ACTIVE model ONLY (it
    // carries the ollama endpoint hash when ollama is active). It must NOT be returned for a
    // non-active model: `fastembed_operational_status` asks for FASTEMBED_MODEL_ID's version even
    // when FastEmbed is inactive, and comparing existing FastEmbed rows against `hash-v1` (or the
    // ollama endpoint hash) would spuriously report them stale/missing. For a non-active model,
    // return its OWN static `spec.version`. The reconcile/active path always queries the active
    // model, so it still gets the dynamic meta.
    if model_id == active_embedding_model_id(conn)?
        && let Some(version) = repo_meta(conn, ACTIVE_EMBEDDING_MODEL_VERSION_META)?
    {
        return Ok(version);
    }
    Ok(default_model_version(model_id).to_string())
}

pub(crate) fn default_model_version(model_id: &str) -> &'static str {
    spec(model_id).map_or("v1", |s| s.version)
}

pub(crate) fn current_embedding_count(conn: &Connection, model_id: &str) -> anyhow::Result<u64> {
    ensure_model_manifest(conn)?;
    let model_version = active_embedding_model_version(conn, model_id)?;
    let count: i64 = conn.query_row(
        "
        SELECT COUNT(*)
        FROM chunk_embeddings
        JOIN chunks ON chunks.id = chunk_embeddings.chunk_id
        -- Scope through the active `files` view (temp.files: repo_id + live generation), exactly \
         as
        -- the `current_artifact_count` sibling does — otherwise this counts every repo's and every
        -- staged generation's embeddings on a consolidated DB. Without a connection context, \
         `files`
        -- falls back to the base table (all commits), preserving the single-repo behavior.
        JOIN files ON files.id = chunks.file_id
        JOIN ai_models ON ai_models.model_id = chunk_embeddings.model_id
        WHERE chunk_embeddings.model_id = ?1
          AND ai_models.installed = 1
          AND ai_models.disabled = 0
          AND ai_models.status = 'Ready'
          AND chunk_embeddings.embedding_dim = ai_models.embedding_dim
          AND chunk_embeddings.status = 'Current'
          AND chunk_embeddings.source_text_hash = chunks.text_hash
          AND chunk_embeddings.model_version = ?2
          AND chunk_embeddings.embedding_text_version = ?3
          AND chunk_embeddings.input_hash != ''
        ",
        params![model_id, model_version, EMBEDDING_TEXT_VERSION],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

pub(crate) fn validate_ready_model(model: &ModelInfo) -> anyhow::Result<()> {
    if model.disabled {
        anyhow::bail!("model {} is disabled", model.model_id);
    }
    if !model.installed || model.status != "Ready" {
        anyhow::bail!("{}", model_not_ready_reason(model));
    }
    let expected_dim = expected_dim(&model.model_id)
        .ok_or_else(|| anyhow::anyhow!("unknown embedding model `{}`", model.model_id))?;
    if model.embedding_dim != Some(i64::try_from(expected_dim).unwrap_or(i64::MAX)) {
        anyhow::bail!(
            "model {} dimension mismatch: manifest has {:?}, expected {}",
            model.model_id,
            model.embedding_dim,
            expected_dim
        );
    }
    Ok(())
}

pub(crate) fn model_not_ready_reason(model: &ModelInfo) -> String {
    if model.disabled {
        "Disabled".to_string()
    } else if let Some(last_error) = &model.last_error {
        last_error.clone()
    } else if !model.installed {
        "MissingModel".to_string()
    } else {
        model.status.clone()
    }
}

pub(crate) fn expected_dim(model_id: &str) -> Option<usize> {
    spec(model_id).map(|s| s.dim)
}

pub(crate) fn fastembed_cache_dir() -> PathBuf {
    if let Ok(cache) = std::env::var("RAG_RAT_MODEL_CACHE") {
        return PathBuf::from(cache);
    }
    if let Ok(cache) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(cache).join("rag-rat").join("models");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache").join("rag-rat").join("models");
    }
    // On Windows HOME/XDG_CACHE_HOME are usually unset; land the model cache per-user under
    // %LOCALAPPDATA% rather than per-checkout in the repo-relative fallback below.
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(local).join("rag-rat").join("models");
    }
    PathBuf::from(".rag-rat").join("models")
}

/// Symmetric int8 scalar-quantization range: codes span [-127, 127] (not 128) so a value and its
/// negation map to opposite codes. The largest-magnitude component maps to ±127; the rest round
/// proportionally.
const INT8_QUANT_LEVELS: f32 = 127.0;

/// Decode a stored embedding blob back to f32. TWO on-disk formats coexist, distinguished purely by
/// length (they never collide: `4*dim != dim+4` for any `dim >= 1`):
/// - **legacy f32**: `dim` little-endian f32 (`4*dim` bytes).
/// - **int8 (#112)**: a leading f32 `scale` then `dim` signed-int8 codes (`4 + dim` bytes),
///   dequantized as `v[i] = code[i] * scale`. ~4x smaller on disk/in memory.
///
/// Keeping the f32 reader means an index written before int8 (or a half-reconciled one) still
/// decodes — the formats can be mixed per-row during migration.
pub(crate) fn decode_vector(blob: &[u8], dim: usize) -> Option<Vec<f32>> {
    if blob.len() == dim.checked_mul(4)? {
        let mut out = Vec::with_capacity(dim);
        for bytes in blob.chunks_exact(4) {
            out.push(f32::from_le_bytes(bytes.try_into().ok()?));
        }
        return Some(out);
    }
    if blob.len() == dim.checked_add(4)? {
        let scale = f32::from_le_bytes(blob.get(..4)?.try_into().ok()?);
        let mut out = Vec::with_capacity(dim);
        for &code in &blob[4..] {
            out.push(f32::from(code as i8) * scale);
        }
        return Some(out);
    }
    None
}

/// Encode an embedding as a symmetric int8-quantized blob (#112): one f32 `scale` + one signed byte
/// per dim, ~4x smaller than the f32 blob. `scale = max|v| / 127`; an all-zero vector encodes
/// `scale = 0` + zero codes. The quantization costs well under 1% recall on the L2-normalized
/// embeddings rag-rat stores (measured on the #120 replay eval). [`decode_vector`] reads it back.
pub(crate) fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let max_abs = vector.iter().fold(0.0_f32, |max, value| max.max(value.abs()));
    let scale = if max_abs > 0.0 { max_abs / INT8_QUANT_LEVELS } else { 0.0 };
    let mut out = Vec::with_capacity(4 + vector.len());
    out.extend_from_slice(&scale.to_le_bytes());
    if scale > 0.0 {
        let inv = 1.0 / scale;
        for &value in vector {
            let code = (value * inv).round().clamp(-INT8_QUANT_LEVELS, INT8_QUANT_LEVELS) as i8;
            out.push(code as u8);
        }
    } else {
        out.resize(4 + vector.len(), 0);
    }
    out
}

pub(crate) fn hash_embed_text(text: &str, dim: usize) -> Vec<f32> {
    let mut vector = vec![0.0_f32; dim];
    let tokens = tokens(text);
    for token in &tokens {
        add_feature(&mut vector, token, 1.0);
    }
    for pair in tokens.windows(2) {
        add_feature(&mut vector, &format!("{}::{}", pair[0], pair[1]), 0.6);
    }
    normalize(&mut vector);
    vector
}

pub(crate) fn tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|part| !part.is_empty())
        .flat_map(split_identifier)
        .filter(|part| part.len() > 1)
        .collect()
}

pub(crate) fn split_identifier(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for ch in value.chars() {
        if ch == '_' || ch == '-' {
            if !current.is_empty() {
                parts.push(current.to_ascii_lowercase());
                current.clear();
            }
            previous_lower = false;
            continue;
        }
        if previous_lower && ch.is_uppercase() && !current.is_empty() {
            parts.push(current.to_ascii_lowercase());
            current.clear();
        }
        previous_lower = ch.is_lowercase() || ch.is_ascii_digit();
        current.push(ch);
    }
    if !current.is_empty() {
        parts.push(current.to_ascii_lowercase());
    }
    parts
}

pub(crate) fn add_feature(vector: &mut [f32], feature: &str, weight: f32) {
    let digest = Sha256::digest(feature.as_bytes());
    let index = u16::from_le_bytes([digest[0], digest[1]]) as usize % vector.len();
    let sign = if digest[2] & 1 == 0 { 1.0 } else { -1.0 };
    vector[index] += sign * weight;
}

pub(crate) fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector {
            *value /= norm;
        }
    }
}

pub(crate) fn chunk_count(conn: &Connection) -> anyhow::Result<u64> {
    // Join the active `files` view (temp.files: active worktree overlay UNION active commit)
    // so status counts the chunks reconcile actually works on, not every indexed commit's
    // rows. Without a connection context, `files` falls back to the base table (all commits).
    let count = conn.query_row(
        "SELECT COUNT(*) FROM chunks JOIN files ON files.id = chunks.file_id",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

pub(crate) fn current_artifact_count(
    conn: &Connection,
    capability: &str,
    model_id: &str,
) -> anyhow::Result<u64> {
    let model_version = active_embedding_model_version(conn, model_id)?;
    let sql = artifact_table_sql(
        capability,
        "
        SELECT COUNT(*)
        FROM {table}
        JOIN chunks ON chunks.id = {table}.chunk_id
        JOIN files ON files.id = chunks.file_id
        JOIN ai_models ON ai_models.model_id = {table}.model_id
        WHERE {table}.model_id = ?1
          AND {table}.status = 'Current'
          AND {table}.source_text_hash = chunks.text_hash
          AND {table}.model_version = ?2
          AND {table}.embedding_text_version = ?3
          AND {table}.input_hash != ''
          AND {table}.embedding_dim = ai_models.embedding_dim
    ",
    );
    count_query3(conn, &sql, model_id, &model_version, EMBEDDING_TEXT_VERSION)
}

pub(crate) fn stale_artifact_count(
    conn: &Connection,
    capability: &str,
    model_id: &str,
) -> anyhow::Result<u64> {
    let model_version = active_embedding_model_version(conn, model_id)?;
    let sql = artifact_table_sql(
        capability,
        "
        SELECT COUNT(*)
        FROM {table}
        JOIN chunks ON chunks.id = {table}.chunk_id
        JOIN files ON files.id = chunks.file_id
        JOIN ai_models ON ai_models.model_id = {table}.model_id
        WHERE {table}.model_id = ?1
          AND (
            {table}.source_text_hash != chunks.text_hash
            OR {table}.model_version != ?2
            OR {table}.embedding_text_version != ?3
            OR {table}.input_hash = ''
            OR {table}.embedding_dim != ai_models.embedding_dim
            OR {table}.status = 'Stale'
          )
    ",
    );
    count_query3(conn, &sql, model_id, &model_version, EMBEDDING_TEXT_VERSION)
}

pub(crate) fn status_artifact_count(
    conn: &Connection,
    capability: &str,
    model_id: &str,
    status: ArtifactStatus,
) -> anyhow::Result<u64> {
    let sql = artifact_table_sql(
        capability,
        "
        SELECT COUNT(*)
        FROM {table}
        JOIN chunks ON chunks.id = {table}.chunk_id
        JOIN files ON files.id = chunks.file_id
        WHERE {table}.model_id = ?1 AND {table}.status = ?2
    ",
    );
    let count =
        conn.query_row(&sql, params![model_id, status.as_str()], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

pub(crate) fn count_query3(
    conn: &Connection,
    sql: &str,
    model_id: &str,
    left: &str,
    right: &str,
) -> anyhow::Result<u64> {
    let count = conn.query_row(sql, params![model_id, left, right], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

pub(crate) fn artifact_table_sql(_capability: &str, template: &str) -> String {
    let table = "chunk_embeddings";
    template.replace("{table}", table)
}

pub(crate) fn set_meta(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO index_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub(crate) fn meta(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM index_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()?)
}

/// Delete a GLOBAL meta key from `index_meta` (a no-op when absent) — the `index_meta` counterpart
/// of [`delete_repo_meta`]. Used by the int8 reencode clear path, whose cursor is global (it walks
/// the whole `chunk_embeddings` table beside an already-global done-gate).
pub(crate) fn delete_meta(conn: &Connection, key: &str) -> anyhow::Result<()> {
    conn.execute("DELETE FROM index_meta WHERE key = ?1", [key])?;
    Ok(())
}

/// Read a per-repo meta value (`repo_meta`) for the repo owning `conn` — the repo-scoped twin of
/// [`meta`], resolving the active repo via `schema::active_repo_id` (the scope-context id, falling
/// back to the sole repo). Used for the per-repo keys relocated out of `index_meta` /
/// `reconcile_meta`: the active embedding model, its freshness version, its provisional/remote-
/// config provenance (V040), and the int8 reencode cursor.
pub(crate) fn repo_meta(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    Ok(crate::index::repo_meta(conn, &repo_id, key)?)
}

/// Upsert a per-repo meta value — the repo-scoped twin of [`set_meta`].
pub(crate) fn set_repo_meta(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    crate::index::set_repo_meta(conn, &repo_id, key, value)?;
    Ok(())
}

/// Delete a per-repo meta key from `repo_meta` (a no-op when absent).
pub(crate) fn delete_repo_meta(conn: &Connection, key: &str) -> anyhow::Result<()> {
    let repo_id = crate::index::schema::active_repo_id(conn)?;
    crate::index::delete_repo_meta(conn, &repo_id, key)?;
    Ok(())
}

/// Persist the active remote-embedding connection params as secret-free JSON in the
/// [`ACTIVE_EMBEDDING_REMOTE_CONFIG_META`] meta (#317 task 5). Written at install when an Ollama
/// model is activated; the `conn`-based `active_embedder` reads it back via
/// [`active_remote_config`] so reconcile (chunk-embed) and search (query-embed) reconstruct the
/// SAME remote embedder without threading config through the search path. SECRET-FREE:
/// `RemoteEmbeddingConfig::auth_env` is the env-var NAME, never the token, so the stored JSON
/// contains no secret.
pub(crate) fn set_active_remote_config(
    conn: &Connection,
    remote: &RemoteEmbeddingConfig,
) -> anyhow::Result<()> {
    let json = serde_json::to_string(remote)
        .map_err(|e| anyhow::anyhow!("failed to serialize remote embedding config: {e}"))?;
    // Per-repo since V040 (rejoined the active-model provenance family in `repo_meta`).
    set_repo_meta(conn, ACTIVE_EMBEDDING_REMOTE_CONFIG_META, &json)
}

/// Read the persisted active remote-embedding config (the inverse of [`set_active_remote_config`]).
/// `None` when no remote config has been written (the common local-embedder case). A present-but-
/// malformed value is a hard error — a corrupted meta must surface loudly, not silently fall back
/// to "no remote config" and then bail with the wrong message in the dispatch. The endpoint comes
/// from the persisted config (toml `[remote] endpoint`); there is no runtime endpoint override.
pub(crate) fn active_remote_config(
    conn: &Connection,
) -> anyhow::Result<Option<RemoteEmbeddingConfig>> {
    let Some(json) = repo_meta(conn, ACTIVE_EMBEDDING_REMOTE_CONFIG_META)? else {
        return Ok(None);
    };
    let remote: RemoteEmbeddingConfig = serde_json::from_str(&json).map_err(|e| {
        anyhow::anyhow!(
            "stored remote embedding config (`{ACTIVE_EMBEDDING_REMOTE_CONFIG_META}`) is \
             malformed: {e}"
        )
    })?;
    Ok(Some(remote))
}

/// Drop the persisted remote-embedding config meta — called when a model is installed LOCALLY, so
/// `active_embedder` stops reconstructing an `OpenAiEmbedder` from a stale prior remote install of
/// the same model (a no-op when the meta is already absent).
pub(crate) fn clear_active_remote_config(conn: &Connection) -> anyhow::Result<()> {
    delete_repo_meta(conn, ACTIVE_EMBEDDING_REMOTE_CONFIG_META)
}

pub(crate) fn set_reconcile_meta(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO reconcile_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub(crate) fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Reuse an existing vector for content we've already embedded. Reads the CONTENT-ADDRESSED
/// `embedding_cache` (keyed by `input_hash`, which folds model id + model version + the exact
/// embedding input text), NOT `chunk_embeddings` — so a vector survives its chunk's deletion on
/// reindex and is reusable across reindexes, branches, and worktrees (#357). An empty `input_hash`
/// is never cacheable, so it never reuses.
pub(crate) fn find_existing_embedding(
    conn: &Connection,
    model_id: &str,
    input_hash: &str,
    dim: usize,
) -> anyhow::Result<Option<Vec<f32>>> {
    if input_hash.is_empty() {
        return Ok(None);
    }
    let vector: Option<Vec<u8>> = conn
        .query_row(
            "SELECT vector_blob FROM embedding_cache
             WHERE input_hash = ?2 AND model_id = ?1 AND embedding_dim = ?3
             LIMIT 1",
            params![model_id, input_hash, i64::try_from(dim).unwrap_or(i64::MAX)],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(blob) = vector { Ok(decode_vector(&blob, dim)) } else { Ok(None) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Vec<f32> {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / norm).collect()
    }
    fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    fn schema_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        conn
    }

    fn sample_remote() -> RemoteEmbeddingConfig {
        RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: rag_rat_base::config::RemoteBackend::Ollama,
            endpoint: Some("http://localhost:11434".to_string()),
            cookbook: None,
            query_endpoint: None,
            // The NAME of an env var — never a token. The round-trip test asserts this name is
            // present and no token is.
            auth_env: Some("OLLAMA_TOKEN".to_string()),
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: 384_000,
            request_timeout_s: 60,
        }
    }

    #[test]
    fn active_remote_config_is_none_when_unset() {
        let conn = schema_conn();
        assert_eq!(active_remote_config(&conn).unwrap(), None);
    }

    #[test]
    fn remote_config_meta_round_trips_without_storing_a_token() {
        let conn = schema_conn();
        let remote = sample_remote();
        set_active_remote_config(&conn, &remote).unwrap();

        // Read-back equals what we wrote.
        assert_eq!(active_remote_config(&conn).unwrap(), Some(remote));

        // SECRET-FREE: the serialized JSON carries the auth_env NAME but no token value.
        let json = repo_meta(&conn, ACTIVE_EMBEDDING_REMOTE_CONFIG_META).unwrap().unwrap();
        assert!(json.contains("OLLAMA_TOKEN"), "stores the env-var name: {json}");
        // Belt-and-suspenders: no bearer/token-shaped material leaks into the meta.
        assert!(!json.to_lowercase().contains("bearer"), "no bearer token in meta: {json}");
        assert!(!json.to_lowercase().contains("secret"), "no secret material in meta: {json}");
    }

    #[test]
    fn active_remote_config_errors_on_malformed_meta() {
        // A corrupted meta must surface loudly, not silently degrade to "no remote config".
        let conn = schema_conn();
        set_repo_meta(&conn, ACTIVE_EMBEDDING_REMOTE_CONFIG_META, "{not valid json").unwrap();
        let err = active_remote_config(&conn).expect_err("malformed meta must error");
        assert!(err.to_string().contains("malformed"), "{err}");
    }

    /// Mark `model_id` Ready + active in a manifest-seeded conn (mirrors a real install's DB
    /// state).
    fn activate_model(conn: &Connection, model_id: &str) {
        ensure_model_manifest(conn).unwrap();
        let dim = spec(model_id).unwrap().dim;
        conn.execute(
            "UPDATE ai_models
             SET installed = 1, disabled = 0, status = 'Ready', embedding_dim = ?2
             WHERE model_id = ?1",
            params![model_id, i64::try_from(dim).unwrap()],
        )
        .unwrap();
        set_repo_meta(conn, ACTIVE_EMBEDDING_MODEL_META, model_id).unwrap();
    }

    #[test]
    fn active_embedding_model_version_is_scoped_to_the_active_model() {
        use rag_rat_base::embedding_models::{BGE_SMALL_MODEL_ID, FASTEMBED_MODEL_ID};

        // The fastembed all-minilm model is active with a dynamic (remote) freshness key in the
        // meta.
        let conn = schema_conn();
        activate_model(&conn, FASTEMBED_MODEL_ID);
        let dynamic_key = "fastembed-all-minilm-l6-v2-v1-deadbeefcafef00d";
        set_repo_meta(&conn, ACTIVE_EMBEDDING_MODEL_VERSION_META, dynamic_key).unwrap();

        // The ACTIVE model gets the dynamic meta...
        assert_eq!(active_embedding_model_version(&conn, FASTEMBED_MODEL_ID).unwrap(), dynamic_key,);
        // ...but a NON-active model gets its OWN static spec.version, NOT the active model's key —
        // so fastembed_operational_status doesn't report a non-active model's rows stale
        // against the active model's key.
        let bge_version = active_embedding_model_version(&conn, BGE_SMALL_MODEL_ID).unwrap();
        assert_eq!(bge_version, spec(BGE_SMALL_MODEL_ID).unwrap().version);
        assert_ne!(bge_version, dynamic_key, "must not leak the active key to a non-active model");
    }

    #[test]
    fn embed_query_falls_back_to_lexical_when_the_remote_query_embed_fails() {
        use rag_rat_base::embedding_models::FASTEMBED_MODEL_ID;

        // The active model is served over Ollama (a persisted remote config) whose endpoint points
        // at a CLOSED port: construction succeeds (from_remote_config doesn't connect), but
        // embed_batch fails (connection refused). The QUERY path must degrade to lexical —
        // Ok(None), not Err — so search still returns BM25.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let conn = schema_conn();
        activate_model(&conn, FASTEMBED_MODEL_ID);
        let remote = RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            backend: rag_rat_base::config::RemoteBackend::Ollama,
            endpoint: Some(format!("http://127.0.0.1:{port}")),
            cookbook: None,
            query_endpoint: None,
            auth_env: None,
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: 384_000,
            request_timeout_s: 2,
        };
        set_active_remote_config(&conn, &remote).unwrap();

        let result =
            embed_query(&conn, "some query").expect("must NOT propagate the runtime error");
        assert!(result.is_none(), "a failed remote query embed degrades to lexical (None)");
    }

    #[test]
    fn encode_vector_uses_the_compact_int8_format() {
        // int8 blob = 4-byte scale + one byte per dim — 4x smaller than the 4*dim f32 blob (#112).
        let v = vec![0.1_f32, -0.5, 0.9, -0.2, 0.0, 0.33];
        assert_eq!(encode_vector(&v).len(), 4 + v.len());
    }

    #[test]
    fn int8_round_trip_is_within_one_quantization_step() {
        let v = vec![0.12_f32, -0.5, 0.97, -0.2, 0.0, 0.33, -0.81];
        let decoded = decode_vector(&encode_vector(&v), v.len()).unwrap();
        let step = 0.97 / 127.0; // max|v| / 127
        for (a, b) in v.iter().zip(&decoded) {
            assert!((a - b).abs() <= step + 1e-6, "{a} vs {b} (step {step})");
        }
    }

    #[test]
    fn decode_vector_still_reads_legacy_f32_blobs() {
        // A pre-int8 (or half-reconciled, mixed-format) index stores 4*dim f32 bytes per row; the
        // length-based format detection must still decode those exactly.
        let v = vec![0.1_f32, -0.5, 0.9];
        let f32_blob: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        assert_eq!(f32_blob.len(), 4 * v.len());
        assert_eq!(decode_vector(&f32_blob, v.len()), Some(v));
    }

    #[test]
    fn int8_preserves_cosine_within_tolerance() {
        // The quality-relevant property: dot(query, dequant(chunk)) ≈ dot(query, chunk) for the
        // L2-normalized vectors rag-rat stores. This is why int8 costs <1% recall.
        let chunk = unit(&[0.2, 0.5, -0.3, 0.7, -0.1, 0.4]);
        let query = unit(&[0.3, 0.4, -0.2, 0.6, 0.1, 0.5]);
        let exact = dot_f32(&query, &chunk);
        let dequant = decode_vector(&encode_vector(&chunk), chunk.len()).unwrap();
        let approx = dot_f32(&query, &dequant);
        assert!((exact - approx).abs() < 0.01, "exact {exact} vs int8 {approx}");
    }

    #[test]
    fn encode_vector_handles_all_zero_vector() {
        let v = vec![0.0_f32; 4];
        assert_eq!(decode_vector(&encode_vector(&v), v.len()), Some(v));
    }

    #[test]
    fn registered_embedding_models_have_consistent_dim_and_version() {
        // Every registered model id must resolve a dim and a NON-fallback version string through
        // the table-driven helpers — else the reconcile freshness key (model_version) is
        // wrong and embeddings silently mis-track. Regression guard for `expected_dim` /
        // `default_model_version` now reading the `EMBEDDING_MODELS` registry (#112).
        for spec in rag_rat_base::embedding_models::EMBEDDING_MODELS {
            assert_eq!(
                expected_dim(spec.model_id),
                Some(spec.dim),
                "expected_dim missing/wrong for {}",
                spec.model_id
            );
            assert_ne!(
                default_model_version(spec.model_id),
                "v1",
                "version fell back to the default for {}",
                spec.model_id
            );
        }
        // jina-v2-base-code is the 768-dim code tier; pin its identity explicitly.
        use rag_rat_base::embedding_models::JINA_CODE_MODEL_ID;
        assert_eq!(expected_dim(JINA_CODE_MODEL_ID), Some(768));
        assert_eq!(
            default_model_version(JINA_CODE_MODEL_ID),
            "jinaai/jina-embeddings-v2-base-code-v1"
        );
    }
}
