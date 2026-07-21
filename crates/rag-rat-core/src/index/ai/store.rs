use rag_rat_db::text_compression::{ChunkTextDecoder, ChunkTextRow};

use super::*;

#[cfg(test)]
thread_local! {
    pub(crate) static ESTIMATED_RECONCILE_JOBS_CALLS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_estimated_reconcile_job_calls() {
    ESTIMATED_RECONCILE_JOBS_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn estimated_reconcile_job_calls() -> usize {
    ESTIMATED_RECONCILE_JOBS_CALLS.with(std::cell::Cell::get)
}

/// Map one candidate row to its `CurrentChunk` (with a placeholder `text`) plus the
/// [`ChunkTextRow`] carrying the chunk's stored text (the compressed `chunk_text` blob + `raw_len`;
/// the `chunks.text` column is gone, so the SELECT INNER JOINs `chunk_text`). The real `text` is
/// filled in a post-loop via [`ChunkTextRow::resolve`] — decompress returns `anyhow::Result`, which
/// can't cross this rusqlite closure (#77 Phase 2). The SELECT order is: 0-5 identity, 6-13
/// embedding metadata, 14 blob, 15 raw_len, 16 dict_version, 17-18 stamped policy columns.
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
        embedding_policy: row.get(17)?,
        embedding_priority: row.get(18)?,
        reason: ReconcileReason::Forced,
    };
    let text_row =
        ChunkTextRow { blob: row.get(14)?, raw_len: row.get(15)?, dict_version: row.get(16)? };
    Ok((chunk, text_row))
}

/// Metadata-only row mapper for the streamed count path ([`for_each_embedding_candidate`]): the
/// same identity + embedding-metadata columns as [`current_chunk_row`], but no `chunk_text`
/// payload — `text` stays empty until [`EmbeddingCandidate::ensure_text`] fetches it. The SELECT
/// order is: 0-5 identity, 6-13 embedding metadata, 14-15 stamped policy columns.
fn candidate_metadata_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CurrentChunk> {
    Ok(CurrentChunk {
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
        embedding_policy: row.get(14)?,
        embedding_priority: row.get(15)?,
        reason: ReconcileReason::Forced,
    })
}

/// One streamed candidate row: chunk metadata up front, text on demand. The count-only callers
/// decide most rows from metadata alone (`is_stale_without_text`, the certified stamped policy
/// column), so the text fetch + decompression runs only for rows that reach a text gate — the
/// input-hash recompute on a metadata-fresh chunk, or the FromText policy fallback when the
/// stamped column isn't certified (#816).
pub(crate) struct EmbeddingCandidate<'run, 'conn, 'dicts> {
    pub(crate) chunk: CurrentChunk,
    text_loaded: bool,
    text_stmt: &'run mut rusqlite::Statement<'conn>,
    decoder: &'run mut ChunkTextDecoder<'dicts>,
}

impl EmbeddingCandidate<'_, '_, '_> {
    /// Fetch + decompress this chunk's text on first call (no-op after); `chunk.text` is empty
    /// until then. Callers MUST call this before any text-reading classifier — `needs_embedding`
    /// past its cheap clauses, `job_policy` without a certified stamp, `build_embedding_input`.
    pub(crate) fn ensure_text(&mut self) -> anyhow::Result<()> {
        if self.text_loaded {
            return Ok(());
        }
        // The streamed SELECT INNER JOINs `chunk_text`, so every yielded row has exactly one text
        // row here; the outer statement stays live across the loop, so both reads share one
        // implicit read transaction and cannot diverge.
        let text_row = self.text_stmt.query_row(params![self.chunk.id], |row| {
            Ok(ChunkTextRow { blob: row.get(0)?, raw_len: row.get(1)?, dict_version: row.get(2)? })
        })?;
        self.chunk.text = text_row.resolve(self.decoder)?;
        self.text_loaded = true;
        Ok(())
    }
}

/// The streamed candidate SELECT for [`for_each_embedding_candidate`]. `chunk_text` stays INNER
/// JOINed so the row set is exactly "live chunks with stored text" (#77 Phase 2 — every live chunk
/// has one blob; the old `WHERE chunks.text IS NOT NULL` was dropped as vacuous), but its payload
/// columns are NOT selected: the count-only callers decide most rows from metadata alone, so the
/// text is point-fetched lazily per row ([`EmbeddingCandidate::ensure_text`]) instead of carried
/// through the whole stream (#816).
///
/// Need-first ORDER BY only when a `limit` truncates the walk: the CASE reads LEFT-JOINed
/// `chunk_embeddings` columns, which no index can serve, so SQLite external-sorts the ENTIRE
/// candidate set into a temp b-tree before yielding row 1 — ~210 MB of temp writes in under half
/// an hour of watcher/reconcile passes on a kernel-scale index (#816). An exhaustive
/// (`limit == None`) walk feeds order-independent counts, so the sort bought nothing there; a
/// limited walk keeps it because the order then decides WHICH rows are counted. The real embed
/// ordering lives in [`embedding_candidate_ids`].
pub(crate) fn embedding_candidate_stream_sql(limit: Option<u32>, changed_first: bool) -> String {
    let order_clause = if limit.is_some() {
        let changed_order = if changed_first {
            "chunks.source_revision DESC,"
        } else {
            "chunks.embedding_priority ASC,"
        };
        format!(
            "ORDER BY
          CASE
            WHEN chunk_embeddings.chunk_id IS NULL THEN 0
            WHEN chunk_embeddings.source_text_hash != chunks.text_hash THEN 1
            WHEN chunk_embeddings.status = 'Failed' THEN 2
            ELSE 3
          END,
          {changed_order}
          chunks.id"
        )
    } else {
        String::new()
    };
    format!(
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
               chunks.embedding_policy,
               chunks.embedding_priority
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        LEFT JOIN chunk_embeddings
          ON chunk_embeddings.chunk_id = chunks.id
         AND chunk_embeddings.model_id = ?1
        JOIN chunk_text ON chunk_text.chunk_id = chunks.id
        {order_clause}
        LIMIT ?2
    "
    )
}

/// Stream embedding candidates one at a time WITHOUT collecting the set into a `Vec`, handing each
/// row to `f` as an [`EmbeddingCandidate`] — metadata up front, text on demand. This bounds the
/// resident memory of a full-index pass to one row (plus one chunk's text when a row reaches a
/// text gate), so a count on a kernel-scale index never materializes every candidate's
/// decompressed text at once (#379, #816). Both callers are count-only
/// ([`estimated_reconcile_jobs`], `embedding_reconcile_plan`); the stream is unordered unless a
/// `limit` makes order part of the count's meaning — see [`embedding_candidate_stream_sql`].
/// `limit == None` walks every candidate.
pub(crate) fn for_each_embedding_candidate(
    conn: &Connection,
    model_id: &str,
    limit: Option<u32>,
    changed_first: bool,
    mut f: impl FnMut(&mut EmbeddingCandidate<'_, '_, '_>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // One dict decoder for the whole stream (dict versions loaded once, reused across rows),
    // loaded before the candidate statement starts iterating.
    let dicts = rag_rat_query::chunk_text_dicts(conn)?;
    let mut decoder = ChunkTextDecoder::new(&dicts);
    // The lazy per-row text fetch ([`EmbeddingCandidate::ensure_text`]): a point lookup by
    // chunk_id, prepared once. The unordered stream arrives in chunks.id-adjacent order, so the
    // probes stay near-sequential.
    let mut text_stmt =
        conn.prepare("SELECT blob, raw_len, dict_version FROM chunk_text WHERE chunk_id = ?1")?;
    let mut stmt = conn.prepare(&embedding_candidate_stream_sql(limit, changed_first))?;
    let rows = stmt.query_map(
        params![model_id, limit.map(i64::from).unwrap_or(i64::MAX)],
        candidate_metadata_row,
    )?;
    for row in rows {
        let mut chunk = row?;
        if !model_id.is_empty() {
            chunk.reason = ReconcileReason::Missing;
        }
        f(&mut EmbeddingCandidate {
            chunk,
            text_loaded: false,
            text_stmt: &mut text_stmt,
            decoder: &mut decoder,
        })?;
    }
    Ok(())
}

/// Ordered candidate chunk ids for one reconcile run, fetched ONCE (ids only, no text), need-first
/// exactly like `for_each_embedding_candidate`. The loop walks this list in batches and loads text
/// per batch via [`current_chunks_by_ids`], so each chunk's text is read at most once per run. The
/// old path re-queried *every* candidate's text on *every* batch — O(n²) SQLite work that dominated
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

/// Snapshot the scoped `files` metadata (id/path/language/kind) into an indexed temp table for one
/// reconcile run. The per-batch chunk query ([`current_chunks_by_ids`]) joins THIS table, not the
/// live `files` view: that view is a repo-/generation-scoped `UNION ALL` compound (see
/// `lifecycle.rs`), and SQLite re-evaluates it for EACH correlated `files.id = chunks.file_id`
/// probe — so the per-batch join was O(chunks × files), ~15 ms/chunk and hours of wall-clock at
/// kernel scale (#725). A plain indexed temp table probes in O(log files). Full-scan queries (the
/// candidate-id list, the estimate) are unaffected — the planner materializes the view once for a
/// scan — so only the per-id batch path reads the snapshot.
///
/// Built FROM the view, so the snapshot carries exactly this run's scope, frozen at loop start like
/// `candidate_ids`; per-chunk freshness is still validated against `chunks`/`chunk_embeddings` at
/// selection and write time, so a mid-run file change cannot embed stale text. Refreshed (DROP +
/// CREATE) on every call and left on the connection between runs — nothing outside the embed loop
/// references it. The embed loop MUST call this before its batch loop; the two batch readers are
/// reachable only through that loop (no direct callers), so the table is always present.
pub(crate) fn snapshot_reconcile_scope_files(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS temp.reconcile_scope_files;
         CREATE TEMP TABLE reconcile_scope_files(
             id INTEGER PRIMARY KEY,
             path TEXT NOT NULL,
             language TEXT NOT NULL,
             kind TEXT NOT NULL
         );
         INSERT INTO temp.reconcile_scope_files SELECT id, path, language, kind FROM files;",
    )?;
    Ok(())
}

/// Load full chunk rows (with text + embedding metadata) for a specific set of ids, in the given
/// order. Used per batch so only the chunks about to be considered are materialized. Joins the
/// [`snapshot_reconcile_scope_files`] temp table, NOT the live `files` view — see that helper for
/// the O(chunks × files) scope-view trap this avoids.
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
               chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version,
               chunks.embedding_policy, chunks.embedding_priority
        FROM chunks
        JOIN temp.reconcile_scope_files AS files ON files.id = chunks.file_id
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
    // Mirror for_each_embedding_candidate: a non-empty model_id means this is a real (non-force)
    // run, so the default reason is Missing (the precise reason is recomputed in
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
    #[cfg(test)]
    ESTIMATED_RECONCILE_JOBS_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));

    // STREAM the candidates and count — never materialize EVERY candidate's decompressed text into
    // a `Vec` just to size the backlog (#64 audit; mirrors the streamed
    // `embedding_reconcile_plan`, #379). This runs on the watcher/maintenance gate
    // (`pending_embedding_jobs`), so materializing the whole index here was a per-pass memory
    // peak. The `force` branch queries with an empty model_id (no embedding rows join, so every
    // chunk is a candidate — matching the old `current_chunks`); otherwise use the active model
    // so `needs_embedding` sees each chunk's embedding row. Text is fetched lazily (#816): a
    // metadata-stale chunk is decided without it, and under a certified stamp so is its policy —
    // only a metadata-fresh chunk's input-hash recompute (or the FromText policy fallback) reads
    // the text.
    let (model_id, changed_first) =
        if options.force { ("", false) } else { (scan.model_id, options.changed_first) };
    let mut count = 0_u64;
    for_each_embedding_candidate(conn, model_id, options.limit, changed_first, |candidate| {
        // Freshness FIRST, policy second (#522). Both operands are pure, so `&&` commutes and
        // the count is byte-identical to the old policy-first order — but in steady state most
        // chunks are already `Current`, so the freshness gate resolves false and short-circuits
        // BEFORE `policy_for_job`, whose low-signal gate would otherwise re-parse the chunk
        // with tree-sitter. The parse now runs only for genuinely stale chunks (about to be
        // embedded anyway) or under `--force` (where `options.force` short-circuits the
        // freshness gate). Policy-skipped chunks (generated/tiny/fixture) are always missing,
        // so `is_stale_without_text` decides them on its first (cheap) clause without fetching,
        // building, or hashing the text — the idle gate does not pay an O(text) pass per
        // skipped chunk.
        let stale = options.force
            || is_stale_without_text(&candidate.chunk, scan.model_version, scan.dim)
            || {
                // Metadata-fresh: only the input-hash recompute is left, the one staleness
                // clause that reads the text (#816). `needs_embedding` re-runs the cheap
                // clauses (all false here), so the boolean is byte-identical to the eager path.
                candidate.ensure_text()?;
                needs_embedding(
                    &candidate.chunk,
                    scan.model_id,
                    scan.model_version,
                    scan.dim,
                    scan.max_embedding_chars,
                )
            };
        if !stale {
            return Ok(());
        }
        // The FromText policy fallback re-parses the chunk text; the certified stamped column
        // does not (#530), so only the fallback pays the fetch.
        if !scan.stamped_policy {
            candidate.ensure_text()?;
        }
        if job_policy(&candidate.chunk, scan.max_embedding_chars, scan.stamped_policy).eligible {
            count = count.saturating_add(1);
        }
        Ok(())
    })?;
    Ok(count)
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
        let policy = job_policy(&candidate, scan.max_embedding_chars, scan.stamped_policy);
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
