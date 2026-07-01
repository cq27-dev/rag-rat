use super::*;
use crate::index::text_compression::{ChunkTextDecoder, ChunkTextRow};

pub(crate) fn current_chunks(
    conn: &Connection,
    limit: Option<u32>,
) -> anyhow::Result<Vec<CurrentChunk>> {
    embedding_job_candidates(conn, "", "", 0, limit, false)
}

/// Map one candidate row to its `CurrentChunk` (with a placeholder `text`) plus the
/// [`ChunkTextRow`] carrying the chunk's stored text (the compressed `chunk_text` blob + `raw_len`;
/// the `chunks.text` column is gone, so the SELECT INNER JOINs `chunk_text`). The real `text` is
/// filled in a post-loop via [`ChunkTextRow::resolve`] — decompress returns `anyhow::Result`, which
/// can't cross this rusqlite closure (#77 Phase 2). The SELECT order is: 0-5 identity, 6-13
/// embedding metadata, 14 blob, 15 raw_len, 16 dict_version.
pub(crate) fn current_chunk_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(CurrentChunk, ChunkTextRow)> {
    let chunk = CurrentChunk {
        id: row.get(0)?,
        path: row.get(1)?,
        language: row.get(2)?,
        file_kind: row.get(3)?,
        chunk_kind: row.get(4)?,
        symbol_path: row.get(5)?,
        text: String::new(),
        text_hash: row.get(6)?,
        embedding_status: row.get(7)?,
        source_text_hash: row.get(8)?,
        model_version: row.get(9)?,
        embedding_dim: row.get(10)?,
        input_hash: row.get(11)?,
        embedding_text_version: row.get(12)?,
        next_retry_after_ms: row.get(13)?,
        reason: ReconcileReason::Forced,
    };
    let text_row =
        ChunkTextRow { blob: row.get(14)?, raw_len: row.get(15)?, dict_version: row.get(16)? };
    Ok((chunk, text_row))
}

pub(crate) fn embedding_job_candidates(
    conn: &Connection,
    model_id: &str,
    model_version: &str,
    dim: usize,
    limit: Option<u32>,
    changed_first: bool,
) -> anyhow::Result<Vec<CurrentChunk>> {
    let changed_order = if changed_first {
        "chunks.source_revision DESC,"
    } else {
        "chunks.embedding_priority ASC,"
    };
    // Chunk text comes from the compressed `chunk_text` store (#77 Phase 2); the `chunks.text`
    // column is gone, so INNER JOIN `chunk_text` (every live chunk has exactly one blob). The old
    // `WHERE chunks.text IS NOT NULL` was already dropped as vacuous.
    let sql = format!(
        "
        SELECT chunks.id,
               files.path,
               files.language,
               files.kind,
               chunks.chunk_kind,
               chunks.symbol_path,
               chunks.text_hash,
               chunk_embeddings.status,
               chunk_embeddings.source_text_hash,
               chunk_embeddings.model_version,
               chunk_embeddings.embedding_dim,
               chunk_embeddings.input_hash,
               chunk_embeddings.embedding_text_version,
               chunk_embeddings.next_retry_after_ms,
               chunk_text.blob,
               chunk_text.raw_len,
               chunk_text.dict_version
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        LEFT JOIN chunk_embeddings
          ON chunk_embeddings.chunk_id = chunks.id
         AND chunk_embeddings.model_id = ?1
        JOIN chunk_text ON chunk_text.chunk_id = chunks.id
        ORDER BY
          CASE
            WHEN chunk_embeddings.chunk_id IS NULL THEN 0
            WHEN chunk_embeddings.source_text_hash != chunks.text_hash THEN 1
            WHEN chunk_embeddings.status = 'Failed' THEN 2
            ELSE 3
          END,
          {changed_order}
          chunks.id
        LIMIT ?5
    "
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            model_id,
            model_version,
            i64::try_from(dim).unwrap_or(i64::MAX),
            now_ms(),
            limit.map(i64::from).unwrap_or(i64::MAX)
        ],
        current_chunk_row,
    )?;
    // Decompress in a post-loop with a single dict decoder (versions loaded once per call, not per
    // row) — decompress's anyhow::Result can't cross the rusqlite closure above.
    let collected = collect_rows(rows)?;
    let dicts = crate::query::chunk_text_dicts(conn)?;
    let mut decoder = ChunkTextDecoder::new(&dicts);
    let mut chunks = Vec::with_capacity(collected.len());
    for (mut chunk, text_row) in collected {
        chunk.text = text_row.resolve(&mut decoder)?;
        if !model_id.is_empty() {
            chunk.reason = ReconcileReason::Missing;
        }
        chunks.push(chunk);
    }
    Ok(chunks)
}

/// Ordered candidate chunk ids for one reconcile run, fetched ONCE (ids only, no text), need-first
/// exactly like `embedding_job_candidates`. The loop walks this list in batches and loads text per
/// batch via [`current_chunks_by_ids`], so each chunk's text is read at most once per run. The old
/// path re-queried *every* candidate's text on *every* batch — O(n²) SQLite work that dominated
/// reconcile on large repos (it looked like model time but was query/row-materialization CPU).
pub(crate) fn embedding_candidate_ids(
    conn: &Connection,
    model_id: &str,
    changed_first: bool,
) -> anyhow::Result<Vec<i64>> {
    let changed_order = if changed_first {
        "chunks.source_revision DESC,"
    } else {
        "chunks.embedding_priority ASC,"
    };
    let sql = format!(
        "
        SELECT chunks.id
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        LEFT JOIN chunk_embeddings
          ON chunk_embeddings.chunk_id = chunks.id
         AND chunk_embeddings.model_id = ?1
        ORDER BY
          CASE
            WHEN chunk_embeddings.chunk_id IS NULL THEN 0
            WHEN chunk_embeddings.source_text_hash != chunks.text_hash THEN 1
            WHEN chunk_embeddings.status = 'Failed' THEN 2
            ELSE 3
          END,
          {changed_order}
          chunks.id
        "
    );
    let mut stmt = conn.prepare(&sql)?;
    let ids = stmt
        .query_map(params![model_id], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Load full chunk rows (with text + embedding metadata) for a specific set of ids, in the given
/// order. Used per batch so only the chunks about to be considered are materialized.
pub(crate) fn current_chunks_by_ids(
    conn: &Connection,
    model_id: &str,
    ids: &[i64],
    decoder: &mut ChunkTextDecoder,
) -> anyhow::Result<Vec<CurrentChunk>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    // Chunk text comes from the compressed `chunk_text` store (#77 Phase 2); the `chunks.text`
    // column is gone, so INNER JOIN `chunk_text` (every live chunk has one blob). The caller reuses
    // one dict `decoder` across all batches of a run, so the dict versions are loaded once per run
    // (not per batch).
    let sql = format!(
        "
        SELECT chunks.id, files.path, files.language, files.kind, chunks.chunk_kind,
               chunks.symbol_path, chunks.text_hash,
               chunk_embeddings.status, chunk_embeddings.source_text_hash,
               chunk_embeddings.model_version, chunk_embeddings.embedding_dim,
               chunk_embeddings.input_hash, chunk_embeddings.embedding_text_version,
               chunk_embeddings.next_retry_after_ms,
               chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        LEFT JOIN chunk_embeddings
          ON chunk_embeddings.chunk_id = chunks.id
         AND chunk_embeddings.model_id = ?1
        JOIN chunk_text ON chunk_text.chunk_id = chunks.id
        WHERE chunks.id IN ({placeholders})
        "
    );
    let mut bind: Vec<Value> = Vec::with_capacity(ids.len() + 1);
    bind.push(Value::Text(model_id.to_string()));
    bind.extend(ids.iter().map(|id| Value::Integer(*id)));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(bind), current_chunk_row)?;
    // Decompress in a post-loop — decompress's anyhow::Result can't cross the rusqlite closure.
    // Mirror embedding_job_candidates: a non-empty model_id means this is a real (non-force) run,
    // so the default reason is Missing (the precise reason is recomputed in
    // select_reconcile_batch).
    let mut by_id: std::collections::HashMap<i64, CurrentChunk> =
        std::collections::HashMap::with_capacity(ids.len());
    for (mut chunk, text_row) in collect_rows(rows)? {
        chunk.text = text_row.resolve(decoder)?;
        if !model_id.is_empty() {
            chunk.reason = ReconcileReason::Missing;
        }
        by_id.insert(chunk.id, chunk);
    }
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

pub(crate) fn estimated_reconcile_jobs(
    conn: &Connection,
    scan: &EmbeddingScan<'_>,
    options: &ReconcileOptions,
) -> anyhow::Result<u64> {
    let candidates = if options.force {
        current_chunks(conn, options.limit)?
    } else {
        embedding_job_candidates(
            conn,
            scan.model_id,
            scan.model_version,
            scan.dim,
            options.limit,
            options.changed_first,
        )?
    };
    let count = candidates
        .iter()
        .filter(|candidate| {
            policy_for_job(candidate, scan.max_embedding_chars).eligible
                && (options.force
                    || needs_embedding(
                        candidate,
                        scan.model_id,
                        scan.model_version,
                        scan.dim,
                        scan.max_embedding_chars,
                    ))
        })
        .count();
    Ok(u64::try_from(count).unwrap_or(u64::MAX))
}

/// Build the embedding jobs for one batch of candidate ids. Text is loaded only for `ids` (the
/// current batch), then the per-chunk policy and freshness filters are applied — the single source
/// of truth for "does this chunk need embedding" stays in Rust ([`needs_embedding`]).
pub(crate) fn select_reconcile_batch(
    conn: &Connection,
    scan: &EmbeddingScan<'_>,
    ids: &[i64],
    options: &ReconcileOptions,
    decoder: &mut ChunkTextDecoder,
) -> anyhow::Result<SelectedBatch> {
    // Under --force the candidate ordering does not reflect embedding state, so the empty model_id
    // (matching no chunk_embeddings) keeps every chunk's reason as Forced.
    let model_id = if options.force { "" } else { scan.model_id };
    let candidates = current_chunks_by_ids(conn, model_id, ids, decoder)?;
    let mut jobs = Vec::new();
    for candidate in candidates {
        let policy = policy_for_job(&candidate, scan.max_embedding_chars);
        if !policy.eligible {
            continue;
        }
        if !options.force
            && !needs_embedding(
                &candidate,
                scan.model_id,
                scan.model_version,
                scan.dim,
                scan.max_embedding_chars,
            )
        {
            continue;
        }
        let input = build_embedding_input(&candidate, scan.max_embedding_chars);
        let reason = if options.force {
            ReconcileReason::Forced
        } else {
            candidate.reason(scan.model_version, scan.dim, now_ms(), scan.max_embedding_chars)
        };
        jobs.push(PreparedEmbeddingJob {
            id: candidate.id,
            text_hash: candidate.text_hash,
            input_hash: embedding_input_hash(scan.model_id, scan.model_version, &input.text),
            input_chars: input.chars,
            input_truncated: input.truncated,
            input_text: input.text,
            policy: policy.policy,
            priority: policy.priority,
            reason,
        });
    }
    Ok(SelectedBatch { jobs })
}

pub(crate) fn build_embedding_input(chunk: &CurrentChunk, max_chars: usize) -> EmbeddingInput {
    let mut input = String::new();
    input.push_str("path: ");
    input.push_str(&chunk.path);
    input.push('\n');
    input.push_str("language: ");
    input.push_str(&chunk.language);
    input.push('\n');
    input.push_str("kind: ");
    input.push_str(&chunk.chunk_kind);
    input.push('\n');
    if let Some(symbol_path) = &chunk.symbol_path {
        input.push_str("symbol: ");
        input.push_str(symbol_path);
        input.push('\n');
    }
    input.push_str("body:\n");
    let prefix_chars = input.chars().count();
    let budget = max_chars.saturating_sub(prefix_chars).max(MIN_EMBEDDING_CHARS);
    let (body, truncated) = truncate_chars(&chunk.text, budget);
    input.push_str(&body);
    let chars = input.chars().count();
    EmbeddingInput { text: input, chars, truncated }
}

pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    (text.chars().take(max_chars).collect(), true)
}

pub(crate) fn embedding_input_hash(model_id: &str, model_version: &str, input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(model_version.as_bytes());
    hasher.update(b"\0");
    hasher.update(EMBEDDING_TEXT_VERSION.as_bytes());
    hasher.update(b"\0");
    hasher.update(input.as_bytes());
    let hash = hasher.finalize();
    let mut out = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub(crate) fn write_current_embedding_batch(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    batch: &[PreparedEmbeddingJob],
    vectors: &[Vec<f32>],
) -> anyhow::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let write_result = (|| {
        for (chunk, vector) in batch.iter().zip(vectors) {
            store_embedding(conn, embedder, model_version, chunk, vector)?;
        }
        Ok(())
    })();
    finish_batch_transaction(conn, write_result)
}

pub(crate) fn finish_batch_transaction(
    conn: &Connection,
    result: anyhow::Result<()>,
) -> anyhow::Result<()> {
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        },
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        },
    }
}

pub(crate) fn store_embedding(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    chunk: &PreparedEmbeddingJob,
    vector: &[f32],
) -> anyhow::Result<()> {
    if vector.len() != embedder.dim() {
        anyhow::bail!(
            "embedding dimension mismatch for {}: got {}, expected {}",
            embedder.model_id(),
            vector.len(),
            embedder.dim()
        );
    }
    conn.execute(
        "
        INSERT INTO chunk_embeddings(
            chunk_id, model_id, model_version, source_text_hash, input_hash,
            embedding_text_version, embedding_policy, embedding_priority, input_chars,
            input_truncated, embedding_dim, vector_blob,
            status, attempt_count, last_error_class, next_retry_after_ms, computed_at_ms,
            created_at_ms, last_error
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'Current', 1, NULL, NULL, ?13, \
         ?13, NULL)
        ON CONFLICT(chunk_id, model_id) DO UPDATE SET
            model_version = excluded.model_version,
            source_text_hash = excluded.source_text_hash,
            input_hash = excluded.input_hash,
            embedding_text_version = excluded.embedding_text_version,
            embedding_policy = excluded.embedding_policy,
            embedding_priority = excluded.embedding_priority,
            input_chars = excluded.input_chars,
            input_truncated = excluded.input_truncated,
            embedding_dim = excluded.embedding_dim,
            vector_blob = excluded.vector_blob,
            status = excluded.status,
            attempt_count = chunk_embeddings.attempt_count + 1,
            last_error_class = NULL,
            next_retry_after_ms = NULL,
            computed_at_ms = excluded.computed_at_ms,
            created_at_ms = excluded.created_at_ms,
            last_error = NULL
        ",
        params![
            chunk.id,
            embedder.model_id(),
            model_version,
            chunk.text_hash,
            chunk.input_hash,
            EMBEDDING_TEXT_VERSION,
            chunk.policy,
            chunk.priority,
            i64::try_from(chunk.input_chars).unwrap_or(i64::MAX),
            chunk.input_truncated,
            i64::try_from(embedder.dim()).unwrap_or(i64::MAX),
            encode_vector(vector),
            now_ms()
        ],
    )?;
    // Content-address the vector so it survives this chunk's deletion on the next reindex (#357):
    // keep it in `embedding_cache` keyed by input_hash (which folds model + version + input text),
    // so reconcile can reuse it for identical content across reindexes / branches / worktrees
    // instead of re-embedding. An empty input_hash is not cacheable; a repeat write of the same
    // content just bumps last_used for GC.
    if !chunk.input_hash.is_empty() {
        conn.execute(
            "INSERT INTO embedding_cache(
                 input_hash, model_id, embedding_dim, vector_blob, computed_at_ms, last_used_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(input_hash) DO UPDATE SET last_used_at_ms = excluded.last_used_at_ms",
            params![
                chunk.input_hash,
                embedder.model_id(),
                i64::try_from(embedder.dim()).unwrap_or(i64::MAX),
                encode_vector(vector),
                now_ms()
            ],
        )?;
    }
    Ok(())
}

/// GC grace for `embedding_cache` (#357): a content vector referenced by no live chunk is kept this
/// long past its last use, so switching back to a branch you left recently reuses it instead of
/// re-embedding. Vectors are small (int8-encoded), so a generous window costs little storage.
const EMBEDDING_CACHE_GRACE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Prune `embedding_cache` vectors that BOTH (a) are referenced by no Current `chunk_embeddings` in
/// ANY context — a sibling branch / worktree still using the content keeps it, matching the
/// oracle's global-sweep rule — AND (b) haven't been used within [`EMBEDDING_CACHE_GRACE_MS`]. The
/// content key is `input_hash`. Called from gc; returns rows deleted.
pub(crate) fn prune_embedding_cache_unreferenced(conn: &Connection) -> anyhow::Result<u64> {
    let cutoff = now_ms().saturating_sub(EMBEDDING_CACHE_GRACE_MS);
    let deleted = conn.execute(
        "DELETE FROM embedding_cache
         WHERE last_used_at_ms < ?1
           AND input_hash NOT IN (
               SELECT input_hash FROM chunk_embeddings
               WHERE status = 'Current' AND input_hash != ''
           )",
        params![cutoff],
    )?;
    Ok(u64::try_from(deleted).unwrap_or(0))
}

pub(crate) fn store_failed_embedding(
    conn: &Connection,
    embedder: &dyn Embedder,
    model_version: &str,
    chunk: &PreparedEmbeddingJob,
    error: &str,
) -> anyhow::Result<()> {
    let retry_at = now_ms().saturating_add(60_000);
    conn.execute(
        "
        INSERT INTO chunk_embeddings(
            chunk_id, model_id, model_version, source_text_hash, input_hash,
            embedding_text_version, embedding_policy, embedding_priority, input_chars,
            input_truncated, embedding_dim, vector_blob,
            status, attempt_count, last_error_class, next_retry_after_ms, computed_at_ms,
            created_at_ms, last_error
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, x'', 'Failed', 1, ?12, ?13, NULL, \
         ?14, ?15)
        ON CONFLICT(chunk_id, model_id) DO UPDATE SET
            model_version = excluded.model_version,
            source_text_hash = excluded.source_text_hash,
            input_hash = excluded.input_hash,
            embedding_text_version = excluded.embedding_text_version,
            embedding_policy = excluded.embedding_policy,
            embedding_priority = excluded.embedding_priority,
            input_chars = excluded.input_chars,
            input_truncated = excluded.input_truncated,
            embedding_dim = excluded.embedding_dim,
            vector_blob = x'',
            status = 'Failed',
            attempt_count = chunk_embeddings.attempt_count + 1,
            last_error_class = excluded.last_error_class,
            next_retry_after_ms = excluded.next_retry_after_ms,
            computed_at_ms = NULL,
            created_at_ms = excluded.created_at_ms,
            last_error = excluded.last_error
        ",
        params![
            chunk.id,
            embedder.model_id(),
            model_version,
            chunk.text_hash,
            chunk.input_hash,
            EMBEDDING_TEXT_VERSION,
            chunk.policy,
            chunk.priority,
            i64::try_from(chunk.input_chars).unwrap_or(i64::MAX),
            chunk.input_truncated,
            i64::try_from(embedder.dim()).unwrap_or(i64::MAX),
            "Transient",
            retry_at,
            now_ms(),
            error
        ],
    )?;
    Ok(())
}
