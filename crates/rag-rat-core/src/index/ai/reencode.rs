use super::*;

/// Meta key marking the one-time legacy-f32 → int8 vector re-encode as done. Once set to `"1"`,
/// [`reencode_legacy_vectors_if_needed`] skips the (full-table) detect query on every later
/// maintenance pass. Safe as a run-once gate because new writes are ALWAYS int8 (`encode_vector`),
/// so no fresh f32 rows ever appear after the conversion.
const VECTOR_INT8_REENCODE_DONE_META: &str = "vector_int8_reencode_done";

/// Reconcile-meta key persisting the keyset cursor (the last converted `(chunk_id, model_id)`)
/// across maintenance passes, so a deadline-stopped conversion RESUMES from where it left off
/// instead of rescanning from the table head. Serialized as `"<chunk_id>\n<model_id>"`. Only
/// meaningful while the done-gate ([`VECTOR_INT8_REENCODE_DONE_META`]) is unset.
const VECTOR_INT8_REENCODE_CURSOR_META: &str = "vector_int8_reencode_cursor";

/// Rows converted per transaction. Bounded so a huge index (millions of embeddings) never builds
/// one giant transaction or holds the write lock for the whole table — each batch reads, converts,
/// and writes a chunk, then commits and loops.
const REENCODE_BATCH_SIZE: usize = 4_000;

/// Outcome of a conversion run: how many rows it re-encoded, and whether it ran to the NATURAL end
/// (`completed == true` — a batch came back short, so no f32 rows remain past the cursor) versus
/// being cut short by the deadline (`completed == false`). The wrapper sets the run-once done-gate
/// only when `completed`.
struct ReencodeOutcome {
    converted: usize,
    completed: bool,
}

/// One legacy f32 row to re-encode: its compound key plus the stored vector bytes. `(chunk_id,
/// model_id)` is the `chunk_embeddings` PK and doubles as the keyset cursor.
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
/// converts only the rows still in f32.
///
/// The conversion loop, parameterized by `batch_size` (so tests can drive multi-batch behavior with
/// a tiny size; the public wrapper passes [`REENCODE_BATCH_SIZE`]) and a `deadline` (so the
/// budgeted maintenance path can stop mid-conversion).
///
/// Termination is CURSOR-driven, not "did this batch convert anything"-driven: a `(chunk_id,
/// model_id)` keyset cursor walks the f32 rows in PK order, advancing past every row it reads
/// (whether or not that row was actually converted). So a row skipped by the concurrent-rewrite
/// guard or by a decode failure is simply passed over — never re-selected, never looped — and the
/// loop ends when a batch comes back short of `batch_size`. The cursor also avoids the quadratic
/// rescan a bare `LIMIT` would cause (each batch would otherwise re-scan from the table start past
/// every already-converted row).
///
/// The cursor is PERSISTED (`VECTOR_INT8_REENCODE_CURSOR_META`) after each committed batch and
/// LOADED at the start, so a deadline-stopped run resumes from where it left off on the next call
/// rather than rescanning. `completed` is `true` only when the loop reached the natural end (a
/// short batch = no f32 rows remain); a deadline stop returns `completed == false` with the cursor
/// left persisted.
fn reencode_legacy_f32_blobs_batched(
    conn: &Connection,
    batch_size: usize,
    deadline: Option<Instant>,
) -> anyhow::Result<ReencodeOutcome> {
    let mut total = 0usize;
    // Resume from the persisted cursor if one is stored (a prior deadline-stopped run); otherwise
    // start before every real row — `chunk_id >= ` any actual id, and the empty model_id sorts
    // first, so `(chunk_id, model_id) > (i64::MIN, "")` matches all rows.
    let mut cursor = load_cursor(conn)?;
    loop {
        // Collect a whole batch BEFORE writing — the SELECT statement must be finalized (its borrow
        // of `conn` dropped) before the per-row UPDATE takes the connection.
        let batch = collect_legacy_f32_batch(conn, &cursor, batch_size)?;
        let Some(last) = batch.last() else {
            // No rows past the cursor → conversion is complete.
            return Ok(ReencodeOutcome { converted: total, completed: true });
        };
        // Advance the cursor to the batch's LAST key, so the next SELECT resumes via the PK index
        // instead of rescanning. Computed before the UPDATE (which mutates the rows) from the read
        // values.
        cursor = (last.chunk_id, last.model_id.clone());
        let exhausted = batch.len() < batch_size;
        total += convert_batch(conn, &batch)?;
        // Persist the cursor only after the batch's transaction has committed, so a crash never
        // leaves the cursor ahead of the durably-converted rows (it can lag — those rows are simply
        // re-checked and skipped — but must never skip an unconverted row).
        save_cursor(conn, &cursor)?;
        if exhausted {
            return Ok(ReencodeOutcome { converted: total, completed: true });
        }
        // Deadline check AFTER a committed batch (never mid-batch): stop without marking complete
        // so the next pass resumes from the just-persisted cursor.
        if deadline.is_some_and(|dl| Instant::now() >= dl) {
            return Ok(ReencodeOutcome { converted: total, completed: false });
        }
    }
}

/// Load the persisted keyset cursor, or the start-of-table sentinel `(i64::MIN, "")` when none is
/// stored (first run, or after a completed conversion cleared it). A malformed stored value falls
/// back to the sentinel — re-walking from the head is correct (already-int8 rows are skipped),
/// never wrong.
fn load_cursor(conn: &Connection) -> anyhow::Result<(i64, String)> {
    let Some(raw) = reconcile_meta(conn, VECTOR_INT8_REENCODE_CURSOR_META)? else {
        return Ok((i64::MIN, String::new()));
    };
    let Some((chunk_id, model_id)) = raw.split_once('\n') else {
        return Ok((i64::MIN, String::new()));
    };
    match chunk_id.parse::<i64>() {
        Ok(chunk_id) => Ok((chunk_id, model_id.to_string())),
        Err(_) => Ok((i64::MIN, String::new())),
    }
}

/// Persist the keyset cursor as `"<chunk_id>\n<model_id>"` (model ids never contain a newline).
fn save_cursor(conn: &Connection, cursor: &(i64, String)) -> anyhow::Result<()> {
    set_reconcile_meta(
        conn,
        VECTOR_INT8_REENCODE_CURSOR_META,
        &format!("{}\n{}", cursor.0, cursor.1),
    )
}

/// Read up to `limit` legacy f32 rows past `cursor`, in PK order. The blob-length predicate is the
/// format detector (f32 is `4 * embedding_dim` bytes, int8 is `embedding_dim + 4` — disjoint for
/// `dim >= 1`); the `(chunk_id, model_id) > cursor` keyset resumes past already-walked rows without
/// rescanning, and ORDER BY makes "the batch's last row is the new cursor" well-defined.
fn collect_legacy_f32_batch(
    conn: &Connection,
    cursor: &(i64, String),
    limit: usize,
) -> anyhow::Result<Vec<LegacyVectorRow>> {
    let mut stmt = conn.prepare(
        "SELECT chunk_id, model_id, embedding_dim, vector_blob
         FROM chunk_embeddings
         WHERE length(vector_blob) = 4 * embedding_dim
           AND (chunk_id, model_id) > (?1, ?2)
         ORDER BY chunk_id, model_id
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![cursor.0, cursor.1, i64::try_from(limit).unwrap_or(i64::MAX)],
        |row| {
            let dim: i64 = row.get(2)?;
            Ok(LegacyVectorRow {
                chunk_id: row.get(0)?,
                model_id: row.get(1)?,
                dim: usize::try_from(dim).unwrap_or(0),
                blob: row.get(3)?,
            })
        },
    )?;
    collect_rows(rows)
}

/// Convert one already-collected batch inside a single transaction. Returns the count actually
/// re-encoded.
///
/// CONCURRENT-REWRITE GUARD: another writer (a reconcile re-embedding this chunk) can replace the
/// row's `vector_blob` — and its `source_text_hash`/`input_hash` — between
/// [`collect_legacy_f32_batch`] (the read) and this UPDATE. So the UPDATE is guarded on the
/// ORIGINAL blob (`vector_blob = ?4`): if the row changed concurrently it no longer matches (new
/// writes are always int8), the UPDATE is a no-op, and the row is left in its valid,
/// freshly-embedded state instead of being clobbered with the stale f32 vector. Only rows whose
/// UPDATE affected exactly one row count as converted; a row skipped by the guard, or one whose f32
/// blob fails to decode (corrupt length/`dim` mismatch), is passed over — not an error, so one
/// bad/raced row can't wedge the conversion.
fn convert_batch(conn: &Connection, batch: &[LegacyVectorRow]) -> anyhow::Result<usize> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let mut converted = 0usize;
        for row in batch {
            let Some(vector) = decode_vector(&row.blob, row.dim) else {
                continue;
            };
            let changed = conn.execute(
                "UPDATE chunk_embeddings SET vector_blob = ?1
                 WHERE chunk_id = ?2 AND model_id = ?3 AND vector_blob = ?4",
                params![encode_vector(&vector), row.chunk_id, row.model_id, row.blob],
            )?;
            converted += changed;
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
///
/// `deadline` bounds the work so the conversion honors the maintenance time budget: when the
/// deadline passes mid-conversion the run STOPS, leaves the done-gate UNSET, and persists its
/// keyset cursor — so the next maintenance pass resumes from there (no rescan) rather than redoing
/// the whole table. `None` runs to completion. The gate is set ONLY when the conversion fully
/// completes.
pub(crate) fn reencode_legacy_vectors_if_needed(
    conn: &Connection,
    deadline: Option<Instant>,
) -> anyhow::Result<usize> {
    if meta(conn, VECTOR_INT8_REENCODE_DONE_META)?.is_some() {
        return Ok(0);
    }
    reencode_and_mark_if_complete(conn, deadline)
}

/// FORCE the conversion, IGNORING the run-once gate at the start (the `--reencode-vectors` path) —
/// runs to completion (no deadline) and SETS the gate on success, so the next maintenance pass
/// doesn't do a pointless full table scan it would otherwise skip. May resume from a persisted
/// cursor left by a prior deadline-stopped maintenance run. Returns the number of rows converted.
pub(crate) fn reencode_legacy_vectors_now(conn: &Connection) -> anyhow::Result<usize> {
    reencode_and_mark_if_complete(conn, None)
}

/// Run the conversion (resuming from / persisting the keyset cursor) and mark the run-once gate
/// done ONLY when it ran to the natural end. On a deadline stop the gate stays unset and the cursor
/// remains persisted, so the next call resumes. On completion the cursor key is cleared (it is no
/// longer meaningful, and a future stray re-run starts clean). Shared by the gated and forced entry
/// points — they differ only in whether the gate is CHECKED first and whether a deadline is passed.
fn reencode_and_mark_if_complete(
    conn: &Connection,
    deadline: Option<Instant>,
) -> anyhow::Result<usize> {
    let outcome = reencode_legacy_f32_blobs_batched(conn, REENCODE_BATCH_SIZE, deadline)?;
    if outcome.completed {
        set_meta(conn, VECTOR_INT8_REENCODE_DONE_META, "1")?;
        clear_cursor(conn)?;
    }
    Ok(outcome.converted)
}

/// Drop the persisted keyset cursor once the conversion is complete (it is no longer meaningful).
fn clear_cursor(conn: &Connection) -> anyhow::Result<()> {
    conn.execute("DELETE FROM reconcile_meta WHERE key = ?1", params![
        VECTOR_INT8_REENCODE_CURSOR_META
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `chunk_embeddings` shape — only the columns the re-encode touches, so the test does
    /// not depend on the full schema/bootstrap. `index_meta` is the done-gate store;
    /// `reconcile_meta` is the keyset-cursor store.
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
             CREATE TABLE index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE reconcile_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
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

    /// Run the full conversion (production batch size, no deadline) and return the converted count
    /// — the common shape the basic round-trip/idempotency tests want.
    fn run_to_completion(conn: &Connection) -> usize {
        let outcome = reencode_legacy_f32_blobs_batched(conn, REENCODE_BATCH_SIZE, None).unwrap();
        assert!(outcome.completed, "a no-deadline run must reach the natural end");
        outcome.converted
    }

    #[test]
    fn reencode_converts_f32_row_to_int8_and_is_idempotent() {
        let conn = setup_conn();
        let vector = vec![0.12_f32, -0.5, 0.97, -0.2, 0.0, 0.33, -0.81];
        let dim = vector.len();
        let original = f32_blob(&vector);
        assert_eq!(original.len(), 4 * dim, "fixture must be the 4*dim f32 format");
        insert_row(&conn, 1, "model-a", &original, dim);

        let converted = run_to_completion(&conn);
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
        assert_eq!(run_to_completion(&conn), 0);
    }

    #[test]
    fn reencode_leaves_existing_int8_and_empty_blobs_untouched() {
        let conn = setup_conn();
        // An already-int8 row (4 + dim bytes) and a Failed row's empty blob must NOT be selected:
        // `4*dim` collides with neither `dim+4` (dim>=1) nor `0`.
        let int8 = encode_vector(&[0.1_f32, -0.4, 0.9, -0.2]);
        insert_row(&conn, 1, "model-a", &int8, 4);
        insert_row(&conn, 2, "model-a", b"", 4);

        assert_eq!(run_to_completion(&conn), 0);
        assert_eq!(stored_blob(&conn, 1), int8, "int8 row must be byte-identical");
        assert_eq!(stored_blob(&conn, 2), b"", "empty failed-row blob untouched");
    }

    fn count_remaining_f32(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM chunk_embeddings WHERE length(vector_blob) = 4 * embedding_dim",
            [],
            |row| row.get(0),
        )
        .unwrap()
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
        assert_eq!(run_to_completion(&conn), total);
        // Every row is now int8.
        assert_eq!(count_remaining_f32(&conn), 0);
    }

    #[test]
    fn keyset_cursor_walks_every_row_across_chunk_ids_and_models_with_tiny_batches() {
        // The keyset cursor is `(chunk_id, model_id)`, so a chunk_id with multiple models and many
        // chunk_ids must ALL convert even when the batch boundary splits a chunk_id's models. Drive
        // a tiny batch_size so the loop iterates many times over a handful of rows: any cursor bug
        // (skipping the rest of a chunk_id's models, or re-selecting + looping) shows here.
        let conn = setup_conn();
        let vector = vec![0.5_f32, -0.5, 0.25, -0.25];
        let blob = f32_blob(&vector);
        // 5 chunk_ids, each with 2 model_ids (model-a, model-b) = 10 rows; batch_size 2 ⇒ ≥5
        // passes, and the (chunk_id, model_id) ordering means a batch boundary lands
        // mid-chunk_id.
        for chunk_id in 0..5 {
            for model in ["model-a", "model-b"] {
                insert_row(&conn, chunk_id, model, &blob, vector.len());
            }
        }
        let outcome = reencode_legacy_f32_blobs_batched(&conn, 2, None).unwrap();
        assert_eq!(outcome.converted, 10, "every (chunk_id, model_id) row must convert");
        assert!(outcome.completed, "a no-deadline run reaches the natural end");
        assert_eq!(count_remaining_f32(&conn), 0);
    }

    #[test]
    fn concurrent_rewrite_guard_leaves_a_changed_row_untouched() {
        // Simulate the race: the row was re-embedded (now a different blob) between the read and
        // the UPDATE. We model it by passing a `LegacyVectorRow` whose `blob` (the f32
        // bytes we "read") differs from what is now stored. The blob-guarded UPDATE must
        // NOT match, so the stored row is left exactly as the concurrent writer left it,
        // and it is NOT counted.
        let conn = setup_conn();
        let dim = 4;
        // What the concurrent writer left in place: a valid int8 blob (new writes are always int8).
        let current_int8 = encode_vector(&[0.1_f32, -0.4, 0.9, -0.2]);
        insert_row(&conn, 1, "model-a", &current_int8, dim);

        // The row WE think we read, with a STALE f32 blob that no longer matches what is stored.
        let stale = LegacyVectorRow {
            chunk_id: 1,
            model_id: "model-a".to_string(),
            dim,
            blob: f32_blob(&[0.9_f32, 0.9, 0.9, 0.9]),
        };
        let converted = convert_batch(&conn, std::slice::from_ref(&stale)).unwrap();
        assert_eq!(converted, 0, "guarded UPDATE must not touch a concurrently-changed row");
        assert_eq!(stored_blob(&conn, 1), current_int8, "stored row left as the writer left it");
    }

    #[test]
    fn if_needed_runs_once_then_meta_gate_skips() {
        let conn = setup_conn();
        let vector = vec![0.3_f32, -0.6, 0.9, -0.1];
        insert_row(&conn, 1, "model-a", &f32_blob(&vector), vector.len());

        // First call converts and sets the gate.
        assert_eq!(reencode_legacy_vectors_if_needed(&conn, None).unwrap(), 1);
        assert_eq!(meta(&conn, VECTOR_INT8_REENCODE_DONE_META).unwrap().as_deref(), Some("1"));

        // Insert a fresh f32 row AFTER the gate is set — the second call must be a no-op (it does
        // not even run the detect query), so the new row stays f32. This models the invariant that
        // no new f32 rows ever appear in practice (writes are always int8), so the gate is safe.
        insert_row(&conn, 2, "model-a", &f32_blob(&vector), vector.len());
        assert_eq!(reencode_legacy_vectors_if_needed(&conn, None).unwrap(), 0);
        assert_eq!(stored_blob(&conn, 2).len(), 4 * vector.len());
    }

    #[test]
    fn forced_path_converts_ignoring_gate_then_sets_it() {
        let conn = setup_conn();
        let vector = vec![0.3_f32, -0.6, 0.9, -0.1];
        insert_row(&conn, 1, "model-a", &f32_blob(&vector), vector.len());
        // Pre-set the gate: the forced path must IGNORE it at the start and still convert.
        set_meta(&conn, VECTOR_INT8_REENCODE_DONE_META, "1").unwrap();

        assert_eq!(reencode_legacy_vectors_now(&conn).unwrap(), 1);
        // ... and it leaves the gate set, so a later maintenance pass skips the full scan.
        assert_eq!(meta(&conn, VECTOR_INT8_REENCODE_DONE_META).unwrap().as_deref(), Some("1"));
        assert_eq!(reencode_legacy_vectors_if_needed(&conn, None).unwrap(), 0);
    }

    fn stored_cursor(conn: &Connection) -> Option<String> {
        reconcile_meta(conn, VECTOR_INT8_REENCODE_CURSOR_META).unwrap()
    }

    #[test]
    fn deadline_stop_leaves_gate_unset_and_persists_cursor_then_resumes_to_completion() {
        let conn = setup_conn();
        let vector = vec![0.5_f32, -0.5, 0.25, -0.25];
        let blob = f32_blob(&vector);
        for chunk_id in 0..6 {
            insert_row(&conn, chunk_id, "model-a", &blob, vector.len());
        }

        // An ALREADY-ELAPSED deadline: the loop converts its first batch (the deadline is only
        // checked AFTER a committed batch, so progress is always made), then stops because
        // `Instant::now() >= deadline`. With batch_size 2 over 6 rows it converts exactly 2, leaves
        // the done-gate UNSET, and persists the cursor at the 2nd row's key.
        let past = Instant::now() - std::time::Duration::from_secs(1);
        let outcome = reencode_legacy_f32_blobs_batched(&conn, 2, Some(past)).unwrap();
        assert_eq!(outcome.converted, 2, "one batch runs before the deadline halts the loop");
        assert!(!outcome.completed, "a deadline stop is not a completion");
        assert_eq!(count_remaining_f32(&conn), 4, "the rest are still f32");
        assert_eq!(stored_cursor(&conn).as_deref(), Some("1\nmodel-a"), "cursor at the 2nd row");

        // The gated entry point STOPPED here would NOT set the done-gate (verified via the loop
        // above returning completed=false). A follow-up call with NO deadline resumes FROM THE
        // PERSISTED CURSOR (not a rescan) and finishes the remaining 4 rows, sets the gate, and
        // clears the cursor.
        let converted = reencode_legacy_vectors_if_needed(&conn, None).unwrap();
        assert_eq!(converted, 4, "resumes the remaining rows, not all 6");
        assert_eq!(count_remaining_f32(&conn), 0, "all rows now int8");
        assert_eq!(meta(&conn, VECTOR_INT8_REENCODE_DONE_META).unwrap().as_deref(), Some("1"));
        assert_eq!(stored_cursor(&conn), None, "cursor cleared on completion");
    }

    #[test]
    fn if_needed_with_deadline_completes_and_sets_gate_when_work_fits() {
        // A normal (deadline far in the future) run via the gated entry point still fully converts
        // and sets the done-gate — the deadline plumbing doesn't change the happy path.
        let conn = setup_conn();
        let vector = vec![0.3_f32, -0.6, 0.9, -0.1];
        for chunk_id in 0..3 {
            insert_row(&conn, chunk_id, "model-a", &f32_blob(&vector), vector.len());
        }
        let far = Instant::now() + std::time::Duration::from_secs(3600);
        assert_eq!(reencode_legacy_vectors_if_needed(&conn, Some(far)).unwrap(), 3);
        assert_eq!(count_remaining_f32(&conn), 0);
        assert_eq!(meta(&conn, VECTOR_INT8_REENCODE_DONE_META).unwrap().as_deref(), Some("1"));
        assert_eq!(stored_cursor(&conn), None);
    }
}
