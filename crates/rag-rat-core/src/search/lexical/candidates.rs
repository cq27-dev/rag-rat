use rag_rat_db::text_compression::{self, ChunkTextRow};
use rusqlite::{Connection, params};

use super::{SearchHit, query, scoring};
use crate::index::ai;

/// The bm25 candidate statement. `files` is the per-connection scope VIEW (a two-branch UNION ALL
/// over `main.files`; see `lifecycle::write_scope_view`), which SQLite otherwise flattens into a
/// compound MERGE that runs the whole FTS scan + `chunks`/`chunk_text` probe pipeline once PER
/// BRANCH. Materializing the (small — it is the FILE table, not chunks) scope view once per query
/// collapses the plan to a SINGLE FTS pipeline.
///
/// `ORDER BY` stays on the `bm25()` alias, NOT `chunk_fts.rank`: the alias defers scoring until
/// after the scope join, so out-of-scope matches (sibling repos, superseded generations, worktree
/// shadowing) are never scored, whereas `ORDER BY rank` forces FTS5's internal sorter to score
/// every physical match — a measured regression on consolidated / pre-gc-window DBs. The `LIMIT`
/// still applies AFTER the scope filter, preserving the per-repo recall bound documented at the
/// call site (`search_with_query_embedding`).
///
/// The secondary `chunks.id` sort key makes equal-`bm25()` ties DETERMINISTIC. It is free (the
/// `USE TEMP B-TREE FOR ORDER BY` step already exists) and load-bearing: the caller turns candidate
/// POSITION into a sub-score via `lexical_rank_score(rank)`, and `LIMIT` decides which tied rows
/// survive — so an unstable tie order is an observable ranking change. The pre-materialization
/// direct-`files` join returned ties in scope-branch order (overlay rows before base rows), a
/// fragile plan artifact; a single materialized scan would otherwise return them in rowid order.
/// Pinning `(score, chunks.id)` fixes the order regardless of scan plan (strictly better than the
/// old plan-dependent tie order). Rewrite is otherwise behavior-preserving: same rows, same
/// generation/repo/worktree scoping.
pub(super) fn bm25_candidates_sql(include_generated: bool) -> String {
    let generated_filter = if include_generated { "1 = 1" } else { "scoped_files.generated = 0" };
    format!(
        "
        WITH scoped_files AS MATERIALIZED (
            SELECT id, path, language, kind, generated FROM files
        )
        SELECT chunks.id, scoped_files.path, scoped_files.language, scoped_files.kind,
               chunks.start_line, chunks.end_line, chunks.symbol_path,
               bm25(chunk_fts) AS score,
               chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version
        FROM chunk_fts
        JOIN chunks ON chunks.id = chunk_fts.rowid
        JOIN scoped_files ON scoped_files.id = chunks.file_id
        JOIN chunk_text ON chunk_text.chunk_id = chunks.id
        WHERE chunk_fts MATCH ?1
          AND {generated_filter}
        ORDER BY score, chunks.id
        LIMIT ?2
        "
    )
}

pub(super) fn bm25_candidates(
    conn: &Connection,
    query: &str,
    limit: i64,
    include_generated: bool,
    decoder: &mut text_compression::ChunkTextDecoder,
) -> anyhow::Result<Vec<SearchHit>> {
    let fts_query = query::fts_query(query);
    if fts_query == "\"\"" {
        return Ok(Vec::new());
    }
    let sql = bm25_candidates_sql(include_generated);
    let mut stmt = conn.prepare(&sql)?;
    // Snippet text comes from the compressed store (#77); collect blob + raw_len here and
    // decompress in the post-loop — decompress returns anyhow::Result, which can't cross the
    // rusqlite closure.
    let rows = stmt.query_map(params![fts_query, limit], |row| {
        Ok((
            SearchHit {
                chunk_id: row.get(0)?,
                path: row.get(1)?,
                language: row.get(2)?,
                kind: row.get(3)?,
                start_line: row.get(4)?,
                end_line: row.get(5)?,
                symbol_path: row.get(6)?,
                score: row.get(7)?,
                // Placeholder — RankedHit::finish sets the real mode from the scored components.
                retrieval_mode: String::new(),
                summary: String::new(),
                graph: None,
                score_components: None,
                importance: None,
                distilled_records: Vec::new(),
            },
            ChunkTextRow { blob: row.get(8)?, raw_len: row.get(9)?, dict_version: row.get(10)? },
        ))
    })?;
    let collected = collect_rows(rows)?;
    let mut hits = Vec::with_capacity(collected.len());
    for (mut hit, text_row) in collected {
        hit.summary = query::snippet(&text_row.resolve(decoder)?, query);
        hits.push(hit);
    }
    Ok(hits)
}

/// The vector candidate statement. Same scope-view materialization as `bm25_candidates_sql`: the
/// `files` two-branch view would otherwise double the whole brute-force `chunk_embeddings` scan +
/// blob-decode pipeline under the compound MERGE. Ordering and truncation happen in Rust after
/// cosine scoring, so there is no `ORDER BY` / `LIMIT` here — the CTE only replaces the direct
/// `files` join, leaving the result set (and therefore the Rust-side ranking) byte-identical.
fn vector_candidates_sql(include_generated: bool) -> String {
    let generated_filter = if include_generated { "1 = 1" } else { "scoped_files.generated = 0" };
    format!(
        "
        WITH scoped_files AS MATERIALIZED (
            SELECT id, path, language, kind, generated FROM files
        )
        SELECT chunks.id, scoped_files.path, scoped_files.language, scoped_files.kind,
               chunks.start_line, chunks.end_line, chunks.symbol_path,
               chunk_embeddings.vector_blob, chunk_text.blob, chunk_text.raw_len,
               chunk_text.dict_version
        FROM chunk_embeddings
        JOIN ai_models ON ai_models.model_id = chunk_embeddings.model_id
        JOIN chunks ON chunks.id = chunk_embeddings.chunk_id
        JOIN scoped_files ON scoped_files.id = chunks.file_id
        JOIN chunk_text ON chunk_text.chunk_id = chunks.id
        WHERE chunk_embeddings.model_id = ?1
          AND ai_models.installed = 1
          AND ai_models.disabled = 0
          AND ai_models.status = 'Ready'
          AND ai_models.embedding_dim = ?2
          AND chunk_embeddings.embedding_dim = ai_models.embedding_dim
          AND chunk_embeddings.status = 'Current'
          AND chunk_embeddings.source_text_hash = chunks.text_hash
          AND chunk_embeddings.model_version = ?3
          AND chunk_embeddings.embedding_text_version = ?4
          AND chunk_embeddings.input_hash != ''
          AND {generated_filter}
        "
    )
}

pub(super) fn vector_candidates(
    conn: &Connection,
    query: &str,
    limit: i64,
    include_generated: bool,
    query_embedding: Option<ai::QueryEmbedding>,
    decoder: &mut text_compression::ChunkTextDecoder,
) -> anyhow::Result<Vec<(SearchHit, f32)>> {
    let Some(query_embedding) = query_embedding else {
        return Ok(Vec::new());
    };
    let model_version = ai::active_embedding_model_version(conn, &query_embedding.model_id)?;
    let sql = vector_candidates_sql(include_generated);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            query_embedding.model_id,
            i64::try_from(query_embedding.dim).unwrap_or(i64::MAX),
            model_version,
            ai::EMBEDDING_TEXT_VERSION
        ],
        |row| {
            let vector_blob: Vec<u8> = row.get(7)?;
            Ok((
                SearchHit {
                    chunk_id: row.get(0)?,
                    path: row.get(1)?,
                    language: row.get(2)?,
                    kind: row.get(3)?,
                    start_line: row.get(4)?,
                    end_line: row.get(5)?,
                    symbol_path: row.get(6)?,
                    score: 0.0,
                    // Placeholder — RankedHit::finish sets the real mode from the scored
                    // components.
                    retrieval_mode: String::new(),
                    // Filled from the compressed store in the post-loop (decompress can't cross the
                    // rusqlite closure).
                    summary: String::new(),
                    graph: None,
                    score_components: None,
                    importance: None,
                    distilled_records: Vec::new(),
                },
                vector_blob,
                ChunkTextRow {
                    blob: row.get(8)?,
                    raw_len: row.get(9)?,
                    dict_version: row.get(10)?,
                },
            ))
        },
    )?;
    // Score first (decode + dot), then truncate, THEN decompress only the survivors' snippets. This
    // is a brute-force flat scan, so many rows can clear `similarity > 0`, but only the top `limit`
    // are kept — decompressing snippet text before the truncate would decompress (and discard) all
    // the rest (#77 Phase 2 read-path perf).
    let mut scored: Vec<(SearchHit, f32, ChunkTextRow)> = Vec::new();
    for (hit, vector_blob, text_row) in collect_rows(rows)? {
        let Some(vector) = ai::decode_vector(&vector_blob, query_embedding.dim) else {
            continue;
        };
        let similarity = scoring::dot(&query_embedding.vector, &vector);
        if similarity > 0.0 {
            scored.push((hit, similarity, text_row));
        }
    }
    // Descending similarity, then ASCENDING chunk_id as a deterministic tie-break BEFORE the
    // truncate. The scope-view materialization (vector_candidates_sql) changed the SQL row
    // iteration order feeding this sort, so equal-similarity candidates straddling `limit` would
    // otherwise pick a plan-dependent survivor (the same tie hazard bm25_candidates_sql fixes with
    // `ORDER BY score, chunks.id`). chunk_id is unique, so the top-`limit` window is fully
    // determined regardless of scan plan.
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.chunk_id.cmp(&b.0.chunk_id))
    });
    scored.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let mut hits = Vec::with_capacity(scored.len());
    for (mut hit, similarity, text_row) in scored {
        hit.summary = query::snippet(&text_row.resolve(decoder)?, query);
        hits.push((hit, similarity));
    }
    Ok(hits)
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
