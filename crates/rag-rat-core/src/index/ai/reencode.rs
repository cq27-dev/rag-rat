use super::*;

/// Meta key marking the one-time legacy-f32 → int8 vector re-encode as done. Once set to `"1"`,
/// [`reencode_legacy_vectors_if_needed`] skips the (full-table) detect query on every later
/// maintenance pass. Safe as a run-once gate because new writes are ALWAYS int8 (`encode_vector`),
/// so no fresh f32 rows ever appear after the conversion.
const VECTOR_INT8_REENCODE_DONE_META: &str = "vector_int8_reencode_done";

/// Rows converted per transaction. Bounded so a huge index (millions of embeddings) never builds
/// one giant transaction or holds the write lock for the whole table — each batch reads, converts,
/// and writes a chunk, then commits and loops.
const REENCODE_BATCH_SIZE: usize = 4_000;

/// One legacy f32 row to re-encode: its compound key plus the stored vector bytes.
struct LegacyVectorRow {
    chunk_id: i64,
    model_id: String,
    dim: usize,
    blob: Vec<u8>,
}

/// Re-encode every `chunk_embeddings` row still stored in the legacy f32 format to the compact int8
/// format (#312). This is a FORMAT-ONLY conversion — decode the f32 blob, re-encode it with
/// [`encode_vector`] (now int8); NO model inference runs, so it is cheap and quality-identical to a
/// fresh int8 reconcile.
///
/// The f32 rows are detected purely by blob length: an f32 blob is `4 * embedding_dim` bytes, an
/// int8 blob is `embedding_dim + 4`, and the two never collide for `dim >= 1` (so a freshly-int8
/// row, or a `Failed` row's empty `x''` blob, is never selected). Matches [`decode_vector`]'s
/// length-based format dispatch.
///
/// Works in bounded batches inside per-batch transactions and is IDEMPOTENT + resumable: re-running
/// converts only the rows still in f32; once every row is int8 the detect query returns nothing and
/// it returns `Ok(0)`. Returns the total number of rows converted.
pub(crate) fn reencode_legacy_f32_blobs(conn: &Connection) -> anyhow::Result<usize> {
    let mut total = 0usize;
    loop {
        // Collect a whole batch BEFORE writing — the SELECT statement must be finalized (its borrow
        // of `conn` dropped) before the per-row UPDATE takes the connection. `LIMIT` keeps each
        // batch bounded; the `length(vector_blob) = 4 * embedding_dim` predicate is self-clearing
        // (a converted row stops matching), so the same LIMIT walks the remaining f32 rows each
        // pass.
        let batch = collect_legacy_f32_batch(conn, REENCODE_BATCH_SIZE)?;
        if batch.is_empty() {
            break;
        }
        let converted = convert_batch(conn, &batch)?;
        total += converted;
        // Defensive termination: a non-empty batch that converted nothing means every row in it was
        // un-decodable. That can't happen today — a length-selected row always decodes — but
        // breaking (rather than re-selecting the same stuck rows forever) keeps the loop
        // provably terminating regardless of `decode_vector`'s behavior. Un-convertible
        // rows stay f32; they already decode-fail in search too, so leaving them is
        // correct.
        if converted == 0 {
            break;
        }
    }
    Ok(total)
}

/// Read up to `limit` legacy f32 rows. The blob-length predicate is the format detector: f32 is
/// `4 * embedding_dim` bytes, int8 is `embedding_dim + 4` — disjoint for `dim >= 1`.
fn collect_legacy_f32_batch(
    conn: &Connection,
    limit: usize,
) -> anyhow::Result<Vec<LegacyVectorRow>> {
    let mut stmt = conn.prepare(
        "SELECT chunk_id, model_id, embedding_dim, vector_blob
         FROM chunk_embeddings
         WHERE length(vector_blob) = 4 * embedding_dim
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
        let dim: i64 = row.get(2)?;
        Ok(LegacyVectorRow {
            chunk_id: row.get(0)?,
            model_id: row.get(1)?,
            dim: usize::try_from(dim).unwrap_or(0),
            blob: row.get(3)?,
        })
    })?;
    collect_rows(rows)
}

/// Convert one already-collected batch inside a single transaction. Returns the count successfully
/// re-encoded (a row whose f32 blob fails to decode — e.g. a corrupt length/`dim` mismatch — is
/// skipped, not aborted, so one bad row can't wedge the whole conversion).
fn convert_batch(conn: &Connection, batch: &[LegacyVectorRow]) -> anyhow::Result<usize> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let mut converted = 0usize;
        for row in batch {
            let Some(vector) = decode_vector(&row.blob, row.dim) else {
                continue;
            };
            conn.execute(
                "UPDATE chunk_embeddings SET vector_blob = ?1
                 WHERE chunk_id = ?2 AND model_id = ?3",
                params![encode_vector(&vector), row.chunk_id, row.model_id],
            )?;
            converted += 1;
        }
        Ok(converted)
    })();
    match result {
        Ok(converted) => {
            conn.execute_batch("COMMIT")?;
            Ok(converted)
        },
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        },
    }
}

/// Run the legacy-f32 → int8 conversion the FIRST time after upgrade, then skip cheaply on every
/// later pass via the [`VECTOR_INT8_REENCODE_DONE_META`] gate (so maintenance does not full-scan
/// `chunk_embeddings` every pass). Returns the number of rows converted this call (`0` once the
/// gate is set). Safe to call repeatedly and from the base index only — the meta gate AND the
/// f32-detect query both make a stray double-call a no-op.
pub(crate) fn reencode_legacy_vectors_if_needed(conn: &Connection) -> anyhow::Result<usize> {
    if meta(conn, VECTOR_INT8_REENCODE_DONE_META)?.is_some() {
        return Ok(0);
    }
    let converted = reencode_legacy_f32_blobs(conn)?;
    set_meta(conn, VECTOR_INT8_REENCODE_DONE_META, "1")?;
    Ok(converted)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `chunk_embeddings` shape — only the columns the re-encode touches, so the test does
    /// not depend on the full schema/bootstrap. `index_meta` is the gate store.
    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE chunk_embeddings(
                 chunk_id INTEGER NOT NULL,
                 model_id TEXT NOT NULL,
                 embedding_dim INTEGER NOT NULL,
                 vector_blob BLOB NOT NULL,
                 PRIMARY KEY(chunk_id, model_id)
             );
             CREATE TABLE index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();
        conn
    }

    fn f32_blob(vector: &[f32]) -> Vec<u8> {
        vector.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn insert_row(conn: &Connection, chunk_id: i64, model_id: &str, blob: &[u8], dim: usize) {
        conn.execute(
            "INSERT INTO chunk_embeddings(chunk_id, model_id, embedding_dim, vector_blob)
             VALUES (?1, ?2, ?3, ?4)",
            params![chunk_id, model_id, i64::try_from(dim).unwrap(), blob],
        )
        .unwrap();
    }

    fn stored_blob(conn: &Connection, chunk_id: i64) -> Vec<u8> {
        conn.query_row(
            "SELECT vector_blob FROM chunk_embeddings WHERE chunk_id = ?1",
            params![chunk_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn reencode_converts_f32_row_to_int8_and_is_idempotent() {
        let conn = setup_conn();
        let vector = vec![0.12_f32, -0.5, 0.97, -0.2, 0.0, 0.33, -0.81];
        let dim = vector.len();
        let original = f32_blob(&vector);
        assert_eq!(original.len(), 4 * dim, "fixture must be the 4*dim f32 format");
        insert_row(&conn, 1, "model-a", &original, dim);

        let converted = reencode_legacy_f32_blobs(&conn).unwrap();
        assert_eq!(converted, 1);

        // (a) the stored blob is now the compact int8 format (4-byte scale + one byte per dim).
        let new_blob = stored_blob(&conn, 1);
        assert_eq!(new_blob.len(), 4 + dim);

        // (b) decode of the new blob matches the original within one quantization step.
        let decoded = decode_vector(&new_blob, dim).unwrap();
        let step = 0.97 / 127.0; // max|v| / 127
        for (a, b) in vector.iter().zip(&decoded) {
            assert!((a - b).abs() <= step + 1e-6, "{a} vs {b} (step {step})");
        }

        // (c) a second run converts nothing — once int8, the row no longer matches the detector.
        assert_eq!(reencode_legacy_f32_blobs(&conn).unwrap(), 0);
    }

    #[test]
    fn reencode_leaves_existing_int8_and_empty_blobs_untouched() {
        let conn = setup_conn();
        // An already-int8 row (4 + dim bytes) and a Failed row's empty blob must NOT be selected:
        // `4*dim` collides with neither `dim+4` (dim>=1) nor `0`.
        let int8 = encode_vector(&[0.1_f32, -0.4, 0.9, -0.2]);
        insert_row(&conn, 1, "model-a", &int8, 4);
        insert_row(&conn, 2, "model-a", b"", 4);

        assert_eq!(reencode_legacy_f32_blobs(&conn).unwrap(), 0);
        assert_eq!(stored_blob(&conn, 1), int8, "int8 row must be byte-identical");
        assert_eq!(stored_blob(&conn, 2), b"", "empty failed-row blob untouched");
    }

    #[test]
    fn reencode_walks_multiple_batches() {
        let conn = setup_conn();
        // More rows than one batch so the loop must iterate; build a count that crosses the bound.
        let total = REENCODE_BATCH_SIZE + 37;
        let vector = vec![0.5_f32, -0.5, 0.25, -0.25];
        let blob = f32_blob(&vector);
        for id in 0..total {
            insert_row(&conn, id as i64, "model-a", &blob, vector.len());
        }
        assert_eq!(reencode_legacy_f32_blobs(&conn).unwrap(), total);
        // Every row is now int8.
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunk_embeddings WHERE length(vector_blob) = 4 * \
                 embedding_dim",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn if_needed_runs_once_then_meta_gate_skips() {
        let conn = setup_conn();
        let vector = vec![0.3_f32, -0.6, 0.9, -0.1];
        insert_row(&conn, 1, "model-a", &f32_blob(&vector), vector.len());

        // First call converts and sets the gate.
        assert_eq!(reencode_legacy_vectors_if_needed(&conn).unwrap(), 1);
        assert_eq!(meta(&conn, VECTOR_INT8_REENCODE_DONE_META).unwrap().as_deref(), Some("1"));

        // Insert a fresh f32 row AFTER the gate is set — the second call must be a no-op (it does
        // not even run the detect query), so the new row stays f32. This models the invariant that
        // no new f32 rows ever appear in practice (writes are always int8), so the gate is safe.
        insert_row(&conn, 2, "model-a", &f32_blob(&vector), vector.len());
        assert_eq!(reencode_legacy_vectors_if_needed(&conn).unwrap(), 0);
        assert_eq!(stored_blob(&conn, 2).len(), 4 * vector.len());
    }
}
