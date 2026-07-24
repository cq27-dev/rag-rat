use super::*;

/// Insert edge rows for `generation` with the shared idempotent write discipline (`INSERT OR
/// IGNORE` on the content-key PK). Returns the rows actually inserted. Shared by `flush_batch`
/// (the full build) and the delta pass so the write shape lives in exactly one place. Runs inside
/// the CALLER's transaction.
pub(in super::super) fn insert_edge_rows(
    conn: &Connection,
    generation: i64,
    batch: &[EdgeRow],
) -> anyhow::Result<u64> {
    let mut inserted = 0u64;
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO clone_edges
            (build_generation, a_path, a_start_byte, a_file_sha, b_path, b_start_byte,
             b_file_sha, overlap, a_token_len, b_token_len, similarity, edge_source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    for e in batch {
        inserted += stmt.execute(params![
            generation,
            e.a_path,
            e.a_start_byte,
            e.a_file_sha,
            e.b_path,
            e.b_start_byte,
            e.b_file_sha,
            e.overlap,
            e.a_token_len,
            e.b_token_len,
            e.similarity,
            e.edge_source,
        ])? as u64;
    }
    Ok(inserted)
}

/// Insert every posting group's per-token rows for `generation` (idempotent, content-key PK).
/// Shared by `flush_batch` and the delta pass; runs inside the CALLER's transaction. Returns the
/// number of rows ACTUALLY inserted (the `INSERT OR IGNORE` row-change sum, so a content-key
/// collision is not counted) — the exact delta the delta pass adds to the cached
/// `postings_row_count` (#830).
pub(in super::super) fn insert_posting_groups(
    conn: &Connection,
    generation: i64,
    postings: &[PostingGroup],
) -> anyhow::Result<u64> {
    let mut inserted = 0u64;
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO clone_subblock_postings
            (build_generation, token_hash, path, start_byte, file_sha)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for g in postings {
        let (path, start_byte, file_sha) = &g.anchor;
        for &token_hash in &g.tokens {
            inserted +=
                stmt.execute(params![generation, token_hash, path, start_byte, file_sha])? as u64;
        }
    }
    Ok(inserted)
}

/// Whether `generation`'s postings can be ORDERED for serving (#479): its pinned df epoch
/// exists, or there is nothing to order (a zero-postings generation — a docs-only or
/// fingerprint-less repo — has no order to lose, and refusing it would make `pending_clone_graph`
/// rebuild an already-current empty graph forever). Only "postings exist but the epoch is gone"
/// (a pre-V051 build the backfill's empty-df edge could not cover, a torn epoch) is unservable —
/// every eligibility gate treats that like `postings_written = 0` (fall back / refuse; one full
/// rebuild self-heals).
pub(in super::super) fn clone_df_epoch_serves(
    conn: &Connection,
    generation: i64,
) -> anyhow::Result<bool> {
    if clone_df_epoch_exists(conn, generation)? {
        return Ok(true);
    }
    let has_postings = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM clone_subblock_postings WHERE build_generation = ?1)",
        params![generation],
        |r| r.get::<_, i64>(0),
    )? != 0;
    Ok(!has_postings)
}

/// The STRICT probe: `generation` has pinned epoch rows. The delta pass requires this — not the
/// [`clone_df_epoch_serves`] sentinel — because it may CREATE the generation's first postings,
/// and emitting them under an empty epoch map would leave postings no reader can order (Codex
/// review of this change). An epoch-less generation goes to the full-rebuild path, which pins one.
pub(in super::super) fn clone_df_epoch_exists(
    conn: &Connection,
    generation: i64,
) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM clone_df_epoch WHERE build_generation = ?1)",
        params![generation],
        |r| r.get::<_, i64>(0),
    )? != 0)
}

/// Pin a FRESH generation's df order (#479): copy the active repo's just-refreshed
/// `clone_token_df` (baseline — the only normalizer kind the graph builds) into `clone_df_epoch`
/// for `generation`. Runs only when a Building generation opens fresh (cursor 0); a resumed
/// partial keeps the epoch it opened under, and CASCADE sweeps the rows with the generation.
/// DELETE-first: a torn pass that snapshotted, died before its first checkpoint, and re-entered
/// the fresh branch re-pins against the re-refreshed df instead of tripping the PK.
pub(super) fn snapshot_clone_df_epoch(conn: &Connection, generation: i64) -> anyhow::Result<()> {
    conn.execute("DELETE FROM clone_df_epoch WHERE build_generation = ?1", params![generation])?;
    let df_scope = rag_rat_db::schema::periphery_repo_scope(conn, "clone_token_df")?;
    let df_clause = rag_rat_db::schema::periphery_repo_scope_clause(&df_scope, "clone_token_df");
    conn.execute(
        &format!(
            "INSERT INTO clone_df_epoch(build_generation, token_hash, df)
             SELECT ?1, token_hash, df FROM clone_token_df
             WHERE normalizer_kind = 'baseline'{df_clause}"
        ),
        params![generation],
    )?;
    Ok(())
}

/// The ` AND clone_graph_generations.repo_id = '…'` predicate for the per-repo generation SWEEPS,
/// or `""` on the pre-A5 schema (the generations table is still repo-global). The generation
/// INTEGER stays globally unique (allocated `MAX(generation)+1` over ALL repos), so the transitive
/// `clone_edges` / `clone_subblock_postings` are scoped for free by `build_generation`; only the
/// generation lifecycle sweeps (allocate/build/complete/invalidate) filter `repo_id` so a repo's
/// precompute never touches a sibling's generations. See `schema::periphery_repo_scope`.
pub(in super::super) fn clone_generation_scope_clause(conn: &Connection) -> anyhow::Result<String> {
    let scope = rag_rat_db::schema::periphery_repo_scope(conn, "clone_graph_generations")?;
    Ok(rag_rat_db::schema::periphery_repo_scope_clause(&scope, "clone_graph_generations"))
}

/// The live (Complete) generation row, if one is published.
pub(in super::super) fn live_generation_row(
    conn: &Connection,
) -> anyhow::Result<Option<GenerationRow>> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let Some(live) = rag_rat_db::meta::repo_meta(conn, &repo_id, "clone_graph_live_generation")?
    else {
        return Ok(None);
    };
    let Ok(generation) = live.parse::<i64>() else {
        return Ok(None);
    };
    read_generation(conn, generation)
}

/// Open the Building generation toward `source_revision`: resume it if it already targets this
/// revision + normalizer, otherwise discard any stale Building generation and allocate a fresh one.
pub(super) fn open_building_generation(
    conn: &Connection,
    source_revision: &str,
) -> anyhow::Result<GenerationRow> {
    // Per-repo (A5): resume / discard only THIS repo's Building generation, via a real `repo_id`
    // predicate from V042's `clone_graph_generations.repo_id` — this SUPERSEDES the A3
    // `multiple_real_repos` seam guard that used to gate the whole resume/discard block.
    // `{repo_clause}` empty pre-A5. The MAX(generation) allocation below stays GLOBAL so the
    // generation integer is unique across repos (keeping the transitive edges/postings scoped by
    // build_generation).
    let scope = rag_rat_db::schema::periphery_repo_scope(conn, "clone_graph_generations")?;
    let repo_clause =
        rag_rat_db::schema::periphery_repo_scope_clause(&scope, "clone_graph_generations");
    let existing: Option<GenerationRow> = conn
        .query_row(
            &format!(
                "SELECT generation, source_revision, normalizer_version, cursor_symbol_id, \
                 edges_written, postings_written, delta_files_applied
                   FROM clone_graph_generations WHERE status = 'Building'{repo_clause}
                  ORDER BY generation DESC LIMIT 1"
            ),
            [],
            map_generation_row,
        )
        .ok();
    if let Some(row) = existing {
        if row.source_revision == source_revision
            && row.normalizer_version == NORM_VERSION
            // Resume only a POSTINGS-AWARE partial. A pre-feature Building generation
            // (`postings_written = 0`) has no postings for its already-walked symbols; resuming it
            // would Complete a generation with a permanent postings gap. Discard it instead so the
            // fresh generation below writes postings from symbol 0 (review R2).
            && row.postings_written
        {
            return Ok(row);
        }
        // Stale partial (a reindex landed) OR a pre-feature postings-less partial: discard it
        // (CASCADE drops its edges + postings) and start over.
        conn.execute(
            &format!("DELETE FROM clone_graph_generations WHERE status = 'Building'{repo_clause}"),
            [],
        )?;
    }
    let generation: i64 = conn.query_row(
        "SELECT COALESCE(MAX(generation), 0) + 1 FROM clone_graph_generations",
        [],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO clone_graph_generations
            (generation, status, theta_floor, normalizer_kind, normalizer_version, source_revision,
             cursor_symbol_id, edges_written, postings_written, started_at_ms)
         VALUES (?1, 'Building', ?2, 'baseline', ?3, ?4, 0, 0, 1, ?5)",
        params![generation, CLONE_PRECOMPUTE_THETA, NORM_VERSION, source_revision, now_ms()],
    )?;
    // Stamp the active repo on the just-allocated generation (A5). No-op pre-A5 (no repo_id
    // column).
    if let Some(repo_id) = &scope {
        conn.execute(
            "UPDATE clone_graph_generations SET repo_id = ?1 WHERE generation = ?2",
            params![repo_id, generation],
        )?;
    }
    Ok(GenerationRow {
        generation,
        source_revision: source_revision.to_string(),
        normalizer_version: NORM_VERSION,
        cursor_symbol_id: 0,
        edges_written: 0,
        postings_written: true,
        delta_files_applied: 0,
    })
}

fn read_generation(conn: &Connection, generation: i64) -> anyhow::Result<Option<GenerationRow>> {
    Ok(conn
        .query_row(
            "SELECT generation, source_revision, normalizer_version, cursor_symbol_id, \
             edges_written, postings_written, delta_files_applied
               FROM clone_graph_generations WHERE generation = ?1",
            params![generation],
            map_generation_row,
        )
        .ok())
}

fn map_generation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GenerationRow> {
    Ok(GenerationRow {
        generation: row.get(0)?,
        source_revision: row.get(1)?,
        normalizer_version: row.get(2)?,
        cursor_symbol_id: row.get(3)?,
        edges_written: row.get::<_, i64>(4)? as u64,
        postings_written: row.get::<_, i64>(5)? != 0,
        delta_files_applied: row.get(6)?,
    })
}
