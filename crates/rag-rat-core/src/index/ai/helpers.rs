use super::*;

pub(crate) fn embed_query(
    conn: &Connection,
    query: &str,
) -> anyhow::Result<Option<QueryEmbedding>> {
    ensure_model_manifest(conn)?;
    let Ok(embedder) = active_embedder(conn, None) else {
        return Ok(None);
    };
    embed_query_with(&*embedder, query).map(Some)
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
    if let Some(model_id) = meta(conn, ACTIVE_EMBEDDING_MODEL_META)? {
        return Ok(model_id);
    }
    Ok(HASH_MODEL_ID.to_string())
}

pub(crate) fn active_embedding_model_version(
    conn: &Connection,
    model_id: &str,
) -> anyhow::Result<String> {
    if let Some(version) = reconcile_meta(conn, ACTIVE_EMBEDDING_MODEL_VERSION_META)? {
        return Ok(version);
    }
    Ok(default_model_version(model_id).to_string())
}

pub(crate) fn default_model_version(model_id: &str) -> &'static str {
    match model_id {
        HASH_MODEL_ID => "hash-v1",
        FASTEMBED_MODEL_ID => "fastembed-all-minilm-l6-v2-v1",
        BGE_SMALL_MODEL_ID => "fastembed-bge-small-en-v1.5-v1",
        JINA_CODE_MODEL_ID => "fastembed-jina-v2-base-code-v1",
        MODEL2VEC_MODEL_ID => "model2vec-potion-retrieval-32m-v1",
        _ => "v1",
    }
}

pub(crate) fn current_embedding_count(conn: &Connection, model_id: &str) -> anyhow::Result<u64> {
    ensure_model_manifest(conn)?;
    let model_version = active_embedding_model_version(conn, model_id)?;
    let count: i64 = conn.query_row(
        "
        SELECT COUNT(*)
        FROM chunk_embeddings
        JOIN chunks ON chunks.id = chunk_embeddings.chunk_id
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

pub(crate) fn active_embedder(
    conn: &Connection,
    intra_threads: Option<usize>,
) -> anyhow::Result<Box<dyn Embedder>> {
    let model_id = active_embedding_model_id(conn)?;
    let model = model(conn, &model_id)?;
    validate_ready_model(&model)?;
    match model.model_id.as_str() {
        HASH_MODEL_ID => Ok(Box::new(HashEmbedder)),
        FASTEMBED_MODEL_ID => fastembed_embedder(intra_threads),
        BGE_SMALL_MODEL_ID => bge_small_embedder(intra_threads),
        JINA_CODE_MODEL_ID => jina_code_embedder(intra_threads),
        MODEL2VEC_MODEL_ID => model2vec_embedder(),
        other => anyhow::bail!("unknown active embedding model `{other}`"),
    }
}

pub(crate) fn model2vec_embedder() -> anyhow::Result<Box<dyn Embedder>> {
    #[cfg(feature = "model2vec")]
    {
        Ok(Box::new(Model2VecEmbedder::new()?))
    }
    #[cfg(not(feature = "model2vec"))]
    {
        anyhow::bail!("{}", MODEL2VEC_MISSING_FEATURE_MESSAGE)
    }
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
    match model_id {
        HASH_MODEL_ID => Some(HASH_EMBEDDING_DIM),
        FASTEMBED_MODEL_ID => Some(FASTEMBED_EMBEDDING_DIM),
        BGE_SMALL_MODEL_ID => Some(BGE_SMALL_EMBEDDING_DIM),
        JINA_CODE_MODEL_ID => Some(JINA_CODE_EMBEDDING_DIM),
        MODEL2VEC_MODEL_ID => Some(MODEL2VEC_EMBEDDING_DIM),
        _ => None,
    }
}

pub(crate) fn fastembed_embedder(
    intra_threads: Option<usize>,
) -> anyhow::Result<Box<dyn Embedder>> {
    #[cfg(feature = "fastembed")]
    {
        Ok(Box::new(FastEmbedEmbedder::new(intra_threads)?))
    }
    #[cfg(not(feature = "fastembed"))]
    {
        let _ = intra_threads;
        anyhow::bail!("{}", FASTEMBED_MISSING_FEATURE_MESSAGE)
    }
}

pub(crate) fn bge_small_embedder(
    intra_threads: Option<usize>,
) -> anyhow::Result<Box<dyn Embedder>> {
    #[cfg(feature = "fastembed")]
    {
        Ok(Box::new(FastEmbedEmbedder::new_bge_small(intra_threads)?))
    }
    #[cfg(not(feature = "fastembed"))]
    {
        let _ = intra_threads;
        anyhow::bail!("{}", FASTEMBED_MISSING_FEATURE_MESSAGE)
    }
}

pub(crate) fn jina_code_embedder(
    intra_threads: Option<usize>,
) -> anyhow::Result<Box<dyn Embedder>> {
    #[cfg(feature = "fastembed")]
    {
        Ok(Box::new(FastEmbedEmbedder::new_jina_code(intra_threads)?))
    }
    #[cfg(not(feature = "fastembed"))]
    {
        let _ = intra_threads;
        anyhow::bail!("{}", FASTEMBED_MISSING_FEATURE_MESSAGE)
    }
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

pub(crate) fn decode_vector(blob: &[u8], dim: usize) -> Option<Vec<f32>> {
    if blob.len() != dim.checked_mul(4)? {
        return None;
    }
    let mut out = Vec::with_capacity(dim);
    for bytes in blob.chunks_exact(4) {
        out.push(f32::from_le_bytes(bytes.try_into().ok()?));
    }
    Some(out)
}

pub(crate) fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        out.extend_from_slice(&value.to_le_bytes());
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

pub(crate) fn set_reconcile_meta(conn: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO reconcile_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub(crate) fn reconcile_meta(conn: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM reconcile_meta WHERE key = ?1", [key], |row| row.get(0))
        .optional()?)
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

pub(crate) fn find_existing_embedding(
    conn: &Connection,
    model_id: &str,
    input_hash: &str,
    dim: usize,
) -> anyhow::Result<Option<Vec<f32>>> {
    let vector: Option<Vec<u8>> = conn
        .query_row(
            "SELECT vector_blob FROM chunk_embeddings
         WHERE model_id = ?1 AND input_hash = ?2 AND status = 'Current' AND embedding_dim = ?3
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

    #[test]
    fn registered_embedding_models_have_consistent_dim_and_version() {
        // Every registered model id must resolve a dim and a NON-fallback version string — else the
        // reconcile freshness key (model_version) is wrong and embeddings silently mis-track.
        // Regression guard for the model-id match arms in expected_dim / default_model_version
        // (#112).
        for (id, dim) in [
            (HASH_MODEL_ID, HASH_EMBEDDING_DIM),
            (FASTEMBED_MODEL_ID, FASTEMBED_EMBEDDING_DIM),
            (BGE_SMALL_MODEL_ID, BGE_SMALL_EMBEDDING_DIM),
            (JINA_CODE_MODEL_ID, JINA_CODE_EMBEDDING_DIM),
            (MODEL2VEC_MODEL_ID, MODEL2VEC_EMBEDDING_DIM),
        ] {
            assert_eq!(expected_dim(id), Some(dim), "expected_dim missing/wrong for {id}");
            assert_ne!(
                default_model_version(id),
                "v1",
                "version fell back to the default for {id}"
            );
        }
        // jina-v2-base-code is the 768-dim code tier; pin its identity explicitly.
        assert_eq!(expected_dim(JINA_CODE_MODEL_ID), Some(768));
        assert_eq!(default_model_version(JINA_CODE_MODEL_ID), "fastembed-jina-v2-base-code-v1");
    }
}
