use super::*;

/// Max chars for a memory title (a one-line summary) and body. The body cap is generous on purpose:
/// Invariant / Decision / BugPattern memories are meant to carry the *why* + *how to apply* in
/// detail, and 4 000 forced real content out (the MCP `memory_create`/`memory_update` schemas
/// document these). Enforced in Rust, not the schema, so raising them needs no migration.
pub const MAX_MEMORY_TITLE_LEN: usize = 160;
pub const MAX_MEMORY_BODY_LEN: usize = 8000;

/// Max BYTES for a memory's `payload_json` (the only otherwise-uncapped envelope input). Sized so a
/// memory's full signed `/3` op envelope (kind, title, body, tags, payload) stays comfortably under
/// the op-log's 256 KiB `CONTENT_ENVELOPE_MAX_BYTES` §18a cap, so a create/update can never persist
/// an un-authorable row (#680). Bytes, not chars, because the envelope budget is a byte budget. The
/// oplog crate pins this consistent in a cross-crate test (`content_op_is_authorable`); the
/// dependency only flows oplog → query, never back, so the cap cannot import the envelope constant.
pub const MAX_MEMORY_PAYLOAD_LEN: usize = 128 * 1024;

/// Max BYTES for a typed-edge's free-form `target_anchor` / `target_repo_id` — the only otherwise-
/// uncapped inputs on the edge write path. A resolved node/github anchor is short, but an EXPLICIT
/// cross-repo edge to a not-yet-indexed repo (or a github ref) stores the caller's RAW anchor /
/// repo id verbatim, and it is carried verbatim into the signed `EdgeAdd` op body. Sized so an
/// `EdgeAdd`'s signed `/3` envelope — even with BOTH free-form fields at this cap — stays well
/// under the op-log's 256 KiB `CONTENT_ENVELOPE_MAX_BYTES` §18a cap, so `add_edge` can never
/// persist an un-authorable edge (#680). Bytes, not chars, because the envelope budget is a byte
/// budget; the oplog crate pins this consistent with its authorable bound in
/// `content_op_is_authorable`'s cross-crate test.
pub const MAX_EDGE_ANCHOR_LEN: usize = 8 * 1024;

pub fn memory_by_id(conn: &Connection, memory_id: &str) -> anyhow::Result<Option<RepoMemory>> {
    // Scoped to the active repo (V042): this by-id read guards `memory_get` / `update_memory` /
    // `mark_obsolete` / `rebind_memory`, so an unscoped lookup would let any caller holding a
    // SIBLING repo's memory id read OR mutate that sibling on a consolidated DB (the mutation
    // guards all key on the row this returns). A sibling id resolves to `None` here — "not
    // found" — which the mutation callers surface as a refusal. The upstream list/search
    // readers already scope, so adding the predicate is a no-op there. Phase E's explicit
    // cross-repo READ surfaces arrive with their own APIs; mutation stays repo-bound.
    // `{repo_clause}` empty pre-A5.
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    let Some(mut memory) = conn
        .query_row(
            &format!(
                "
            SELECT id AS memory_id,
                   kind AS kind,
                   title AS title,
                   body AS body,
                   confidence AS confidence,
                   status AS status,
                   created_by AS created_by,
                   created_at_ms AS created_at_ms,
                   updated_at_ms AS updated_at_ms,
                   source AS source,
                   payload_json AS payload_json,
                   source_text_hash AS source_text_hash,
                   input_hash AS input_hash,
                   memory_version AS memory_version
            FROM repo_memories
            WHERE id = ?1{repo_clause}
            "
            ),
            [memory_id],
            memory_row,
        )
        .optional()?
    else {
        return Ok(None);
    };
    attach_memory_children(conn, &mut memory)?;
    Ok(Some(memory))
}
pub fn memories_for_chunk(
    conn: &Connection,
    chunk_id: i64,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    // Two binding kinds answer for one chunk, and they are not equally specific: a chunk binding
    // names THIS code, a path binding names the whole file. The caller's `limit` is a volume cap,
    // so under a shared ranking a file-level note touched today evicts the memory an author
    // anchored to this exact chunk — the more specific binding, and the reason the attachment
    // exists. Specificity leads, recency breaks the tie within each tier.
    //
    // GROUP BY, not DISTINCT: the tier is a PER-ROW expression, so a memory holding BOTH a chunk
    // and a path binding joins twice and DISTINCT would key on (id, tier, updated_at_ms) and return
    // it once per binding. The aggregate is what folds the two rows into one tier value — DISTINCT
    // over the id alone would have collapsed them, but it cannot carry the tier. `IS` (not `=`)
    // keeps a path binding's NULL `chunk_id` scoring 0 rather than NULL, so the tiers stay
    // comparable.
    let mut stmt = conn.prepare(&format!(
        "
        SELECT repo_memories.id AS memory_id,
               MAX(repo_memory_bindings.chunk_id IS ?1) AS binds_this_chunk,
               repo_memories.updated_at_ms AS updated_at_ms
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
         AND repo_memory_bindings.repo_id = repo_memories.repo_id
        LEFT JOIN chunks ON chunks.id = ?1
        LEFT JOIN files ON files.id = chunks.file_id
        WHERE repo_memories.status IN ('active', 'stale'){repo_clause}
          AND (
              repo_memory_bindings.chunk_id = ?1
              OR (files.path IS NOT NULL AND repo_memory_bindings.path = files.path)
          )
        GROUP BY repo_memories.id
        ORDER BY binds_this_chunk DESC, updated_at_ms DESC
        LIMIT ?2
        "
    ))?;
    ids_to_memories(
        conn,
        stmt.query_map(params![chunk_id, i64::from(limit)], |row| {
            row.get::<_, String>("memory_id")
        })?,
    )
}
pub fn memories_for_path(
    conn: &Connection,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    let mut stmt = conn.prepare(&format!(
        "
        SELECT DISTINCT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
         AND repo_memory_bindings.repo_id = repo_memories.repo_id
        WHERE repo_memories.status IN ('active', 'stale'){repo_clause}
          AND repo_memory_bindings.path = ?1
        ORDER BY repo_memories.updated_at_ms DESC
        LIMIT ?2
        "
    ))?;
    ids_to_memories(
        conn,
        stmt.query_map(params![path, i64::from(limit)], |row| row.get("memory_id"))?,
    )
}
pub fn memories_for_symbol(
    conn: &Connection,
    symbol: &crate::symbol::SymbolHit,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let chunk_ids = chunk_ids_for_symbol(conn, symbol)?;
    let mut candidate_ids = BTreeSet::new();
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    let mut stmt = conn.prepare(&format!(
        "
        SELECT DISTINCT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
         AND repo_memory_bindings.repo_id = repo_memories.repo_id
        WHERE repo_memories.status IN ('active', 'stale'){repo_clause}
          AND (
              repo_memory_bindings.logical_symbol_id = ?1
              OR repo_memory_bindings.symbol_id = ?2
              OR repo_memory_bindings.binding_id = ?3
              OR (
                  repo_memory_bindings.binding_kind = 'path'
                  AND repo_memory_bindings.path = ?4
              )
          )
        ORDER BY repo_memories.updated_at_ms DESC
        LIMIT ?5
        "
    ))?;
    let rows = stmt.query_map(
        params![
            symbol.logical_symbol_id,
            symbol.symbol_id,
            symbol.qualified_name,
            symbol.path,
            i64::from(limit)
        ],
        |row| row.get::<_, String>("memory_id"),
    )?;
    for row in rows {
        candidate_ids.insert(row?);
    }
    if !chunk_ids.is_empty() {
        let placeholders = std::iter::repeat_n("?", chunk_ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "
            SELECT DISTINCT repo_memories.id AS memory_id
            FROM repo_memories
            JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
             AND repo_memory_bindings.repo_id = repo_memories.repo_id
            WHERE repo_memories.status IN ('active', 'stale'){repo_clause}
              AND repo_memory_bindings.chunk_id IN ({placeholders})
            ORDER BY repo_memories.updated_at_ms DESC
            LIMIT ?
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut values =
            chunk_ids.iter().map(|id| rusqlite::types::Value::Integer(*id)).collect::<Vec<_>>();
        values.push(rusqlite::types::Value::Integer(i64::from(limit)));
        let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
            row.get::<_, String>("memory_id")
        })?;
        for row in rows {
            candidate_ids.insert(row?);
        }
    }
    let mut memories = Vec::new();
    for id in candidate_ids.into_iter().take(usize::try_from(limit).unwrap_or(usize::MAX)) {
        // `drive_by_memory`, not `memory_by_id`: this reader collects ids into a set rather than
        // going through `ids_to_memories`, so it has to ask for the drive-by hydration explicitly
        // or it would be the one `memories_for_*` surface that never marks anchor drift (#1236).
        if let Some(memory) = drive_by_memory(conn, &id)? {
            memories.push(memory);
        }
    }
    memories.sort_by_key(|memory| std::cmp::Reverse(memory.updated_at_ms));
    Ok(memories)
}
pub fn memory_evidence_for_symbol(
    conn: &Connection,
    symbol: &crate::symbol::SymbolHit,
    limit: u32,
) -> anyhow::Result<RepoMemoryEvidence> {
    let (direct, stale) = split_active_stale(memories_for_symbol(conn, symbol, limit)?);
    Ok(RepoMemoryEvidence {
        direct,
        path_crossed: Vec::new(),
        call_path_crossed: Vec::new(),
        stale,
    })
}
pub fn memory_evidence_for_symbol_and_edges(
    conn: &Connection,
    symbol: &crate::symbol::SymbolHit,
    caller_edge_ids: &[i64],
    callee_edge_ids: &[i64],
    limit: u32,
) -> anyhow::Result<(RepoMemoryEvidence, bool)> {
    let mut all_edges = caller_edge_ids.to_vec();
    all_edges.extend_from_slice(callee_edge_ids);
    // Each lane is independently capped at `limit` by its query. Detect truncation from the
    // PRE-split row counts: `split_active_stale` partitions a lane into active + stale, so an
    // active lane can sit below `limit` even though the query was capped (rows went to stale) —
    // counting active lanes alone misses that (#146 review). A lane that returned `limit` rows
    // may hide more.
    let direct_rows = memories_for_symbol(conn, symbol, limit)?;
    let edge_rows = memories_for_edges(conn, &all_edges, limit)?;
    let call_path_rows =
        call_path_memories_for_crossed(conn, caller_edge_ids, callee_edge_ids, limit)?;
    let cap = limit as usize;
    let truncated = cap != 0
        && (direct_rows.len() >= cap || edge_rows.len() >= cap || call_path_rows.len() >= cap);

    let (direct, mut stale) = split_active_stale(direct_rows);
    let (path_crossed, crossed_stale) = split_active_stale(edge_rows);
    stale.extend(crossed_stale);
    let (call_path_crossed, call_path_stale) = split_active_stale(call_path_rows);
    stale.extend(call_path_stale);
    Ok((RepoMemoryEvidence { direct, path_crossed, call_path_crossed, stale }, truncated))
}
/// Surface call-path memories whose server-derived hash this traversal crossed: compute the
/// single-edge hash for every crossed edge and the two-edge `caller -> callee` hash for each
/// caller/callee pairing through the focus symbol, then look them up. Both sides are capped so
/// the pairing stays bounded; non-matching hashes simply find nothing (no false positives — the
/// hash is content-derived). (#38)
pub(crate) fn call_path_memories_for_crossed(
    conn: &Connection,
    caller_edge_ids: &[i64],
    callee_edge_ids: &[i64],
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    const MAX_SIDE: usize = 16;
    let fingerprints = |ids: &[i64]| -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        for &edge_id in ids.iter().take(MAX_SIDE) {
            if let Some(edge) = call_path_edge_by_id(conn, edge_id)? {
                out.push(edge.fingerprint);
            }
        }
        Ok(out)
    };
    let caller_fps = fingerprints(caller_edge_ids)?;
    let callee_fps = fingerprints(callee_edge_ids)?;

    let mut hashes = std::collections::BTreeSet::new();
    for fingerprint in caller_fps.iter().chain(callee_fps.iter()) {
        hashes.insert(compute_edge_sequence_hash([fingerprint.as_str()]));
    }
    for caller_fp in &caller_fps {
        for callee_fp in &callee_fps {
            hashes.insert(compute_edge_sequence_hash([caller_fp.as_str(), callee_fp.as_str()]));
        }
    }
    if hashes.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = std::iter::repeat_n("?", hashes.len()).collect::<Vec<_>>().join(",");
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    let sql = format!(
        "
        SELECT DISTINCT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_call_paths ON repo_memory_call_paths.memory_id = repo_memories.id
        WHERE repo_memories.status IN ('active', 'stale'){repo_clause}
          AND repo_memory_call_paths.edge_sequence_hash IN ({placeholders})
        ORDER BY repo_memories.updated_at_ms DESC
        LIMIT ?
        "
    );
    let mut values =
        hashes.iter().map(|hash| rusqlite::types::Value::Text(hash.clone())).collect::<Vec<_>>();
    values.push(rusqlite::types::Value::Integer(i64::from(limit)));
    let mut stmt = conn.prepare(&sql)?;
    ids_to_memories(
        conn,
        stmt.query_map(rusqlite::params_from_iter(values), |row| row.get("memory_id"))?,
    )
}
pub fn memories_for_edges(
    conn: &Connection,
    edge_ids: &[i64],
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    if edge_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut unique_edge_ids = edge_ids.to_vec();
    unique_edge_ids.sort_unstable();
    unique_edge_ids.dedup();
    let placeholders =
        std::iter::repeat_n("?", unique_edge_ids.len()).collect::<Vec<_>>().join(",");
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    let sql = format!(
        "
        SELECT DISTINCT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
         AND repo_memory_bindings.repo_id = repo_memories.repo_id
        WHERE repo_memories.status IN ('active', 'stale'){repo_clause}
          AND repo_memory_bindings.edge_id IN ({placeholders})
        ORDER BY repo_memories.updated_at_ms DESC
        LIMIT ?
        "
    );
    let mut values =
        unique_edge_ids.iter().map(|id| rusqlite::types::Value::Integer(*id)).collect::<Vec<_>>();
    values.push(rusqlite::types::Value::Integer(i64::from(limit)));
    let mut stmt = conn.prepare(&sql)?;
    ids_to_memories(
        conn,
        stmt.query_map(rusqlite::params_from_iter(values), |row| row.get("memory_id"))?,
    )
}
pub fn memories_for_call_path_hash(
    conn: &Connection,
    edge_sequence_hash: &str,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    let mut stmt = conn.prepare(&format!(
        "
        SELECT DISTINCT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_call_paths ON repo_memory_call_paths.memory_id = repo_memories.id
        WHERE repo_memories.status IN ('active', 'stale'){repo_clause}
          AND repo_memory_call_paths.edge_sequence_hash = ?1
        ORDER BY repo_memories.updated_at_ms DESC
        LIMIT ?2
        "
    ))?;
    ids_to_memories(
        conn,
        stmt.query_map(params![edge_sequence_hash, i64::from(limit)], |row| row.get("memory_id"))?,
    )
}
pub fn memory_search(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    Ok(memory_search_scored(conn, query, limit)?.into_iter().map(|(memory, _)| memory).collect())
}

/// Keyword search over active+stale memories, best match first, carrying each hit's raw
/// `bm25(repo_memory_fts)` rank so a caller can gate on RELATIVE relevance (the plain
/// [`memory_search`] drops it).
///
/// INVARIANT: the returned score is SQLite's bm25, which is NEGATIVE and lower-is-better — a
/// stronger match is MORE negative. Callers comparing scores must invert first; treating the
/// raw value as a higher-is-better strength inverts the ranking.
pub fn memory_search_scored(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<(RepoMemory, f64)>> {
    let query = fts_query(query);
    if query.is_empty() {
        return Ok(Vec::new());
    }
    // Isolation: the FTS MATCH spans every repo, so the join to `repo_memories` filters to the
    // active repo. `{repo_clause}` is empty pre-A5 (memory still repo-global). This is the
    // cross-repo memory search leak guard — an identical-titled memory in a sibling repo must
    // never surface here.
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    // GROUP BY, not DISTINCT: selecting the score alongside the id makes the DISTINCT key the
    // (memory_id, bm25) PAIR, so a stray duplicate FTS row for one memory — an interrupted heal, an
    // import that inserts before its scoped DELETE — scores differently and survives it, and then
    // consumes a LIMIT slot that a distinct memory should have had. Grouping collapses the
    // duplicates BEFORE the LIMIT, so `limit` still means `limit` distinct memories, and MIN takes
    // the best of the duplicate rows (bm25 is negative, lower is better). The bm25 call must be
    // scored in a MATERIALIZED CTE: fts5 rejects its auxiliary functions inside an aggregate, and
    // an inline subquery gets flattened back into the aggregate query and rejected the same way.
    let mut stmt = conn.prepare(&format!(
        "
        WITH scored AS MATERIALIZED (
            SELECT repo_memory_fts.memory_id AS memory_id,
                   bm25(repo_memory_fts) AS bm25_rank
            FROM repo_memory_fts
            JOIN repo_memories ON repo_memories.id = repo_memory_fts.memory_id
            WHERE repo_memory_fts MATCH ?1
              AND repo_memories.status IN ('active', 'stale'){repo_clause}
        )
        SELECT memory_id, MIN(bm25_rank) AS bm25_rank
        FROM scored
        GROUP BY memory_id
        ORDER BY bm25_rank
        LIMIT ?2
        "
    ))?;
    let ranked = stmt
        .query_map(params![query, i64::from(limit)], |row| {
            Ok((row.get::<_, String>("memory_id")?, row.get::<_, f64>("bm25_rank")?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut hits = Vec::with_capacity(ranked.len());
    for (memory_id, rank) in ranked {
        if let Some(memory) = memory_by_id(conn, &memory_id)? {
            hits.push((memory, rank));
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod memory_search_dedup_tests {
    use super::*;

    /// A corpus where one memory's FTS mirror carries a stray SECOND row — what an interrupted
    /// heal, or an import that inserts before its scoped DELETE, leaves behind. The bodies differ,
    /// so the two rows score differently and a `DISTINCT` keyed on the id + score pair keeps both.
    fn conn_with_duplicated_fts_row() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('r', 'r', 0)",
            [],
        )
        .unwrap();
        let insert_memory = |id: &str, body: &str| {
            conn.execute(
                "INSERT INTO repo_memories(id, kind, title, body, confidence, status,
                        created_at_ms, updated_at_ms, source, memory_version, repo_id)
                 VALUES (?1, 'Invariant', 'Quokkaform routing', ?2, 'high', 'active', 0, 0,
                         'agent', 'v1', 'r')",
                [id, body],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
                 VALUES ('r', ?1, 'Quokkaform routing', ?2, 'Invariant', '')",
                [id, body],
            )
            .unwrap();
        };
        // `m` is the strongest match by term frequency, so BOTH of its rows outrank the others.
        insert_memory("m", "quokkaform quokkaform quokkaform");
        for other in ["m2", "m3", "m4"] {
            insert_memory(other, "quokkaform is pinned by the router on every rebuild");
        }
        // The stray duplicate mirror row for `m`, scoring differently from its real one.
        conn.execute(
            "INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
             VALUES ('r', 'm', 'Quokkaform routing', 'quokkaform quokkaform quokkaform rebuild',
                     'Invariant', '')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn duplicate_fts_rows_collapse_to_one_hit_per_memory() {
        let conn = conn_with_duplicated_fts_row();
        let hits = memory_search(&conn, "quokkaform", 10).unwrap();
        assert_eq!(hits.len(), 4, "one hit per memory, not one per FTS row: {hits:?}");
    }

    /// The duplicate must not eat a result slot: `limit` counts distinct memories, so it has to be
    /// applied AFTER the duplicate rows collapse, not to the raw FTS row set.
    ///
    /// The DISTINCTNESS of the ids is the whole claim — a row count of 3 alone is exactly what the
    /// rejected `DISTINCT (memory_id, bm25)` shape returns, with `m` twice and one memory pushed
    /// out.
    #[test]
    fn a_duplicate_fts_row_does_not_consume_a_limit_slot() {
        let conn = conn_with_duplicated_fts_row();
        let hits = memory_search(&conn, "quokkaform", 3).unwrap();
        let ids: BTreeSet<&str> = hits.iter().map(|hit| hit.memory_id.as_str()).collect();
        assert_eq!(hits.len(), 3, "the limit is spent in full: {hits:?}");
        assert_eq!(ids.len(), 3, "limit counts distinct memories, not FTS rows: {hits:?}");
    }
}

/// Flat summary of one repo memory — boundary DTO for the CLI `memory list` surface.
///
/// One row per memory; the binding fields reflect the first/primary binding row
/// (ORDER BY binding_kind, binding_id LIMIT 1 per memory).
#[derive(Debug, Clone, Serialize)]
pub struct MemorySummary {
    pub memory_id: String,
    pub kind: String,
    pub title: String,
    pub status: String,
    pub binding_kind: String,
    pub binding_id: String,
}

/// Read-only list of active+stale memories, optionally filtered by binding_kind.
///
/// Invariant: purely READ — never writes to the database.
/// Each memory appears once; the primary binding is the first row when bindings are
/// ordered by (binding_kind, binding_id) — stable and deterministic.
pub fn list_memories(conn: &Connection, kind: Option<&str>) -> anyhow::Result<Vec<MemorySummary>> {
    // Use a subquery to pick the first binding per memory (stable tie-break).
    // The outer WHERE on m.status restricts to non-obsolete memories, matching
    // the memory_search / memories_for_* convention.
    let scope = memory_repo_scope(conn)?;
    let repo_clause = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "m");
    let rows: Vec<MemorySummary> = if let Some(binding_kind) = kind {
        let mut stmt = conn.prepare(&format!(
            "
            SELECT m.id AS memory_id, m.kind, m.title, m.status,
                   b.binding_kind, b.binding_id
            FROM repo_memories AS m
            JOIN repo_memory_bindings AS b ON b.memory_id = m.id AND b.repo_id = m.repo_id
            WHERE m.status IN ('active', 'stale'){repo_clause}
              AND b.binding_kind = ?1
              AND b.rowid = (
                  SELECT b2.rowid FROM repo_memory_bindings AS b2
                  WHERE b2.memory_id = m.id AND b2.repo_id = m.repo_id
                  ORDER BY b2.binding_kind, b2.binding_id
                  LIMIT 1
              )
            ORDER BY m.updated_at_ms DESC
            "
        ))?;
        stmt.query_map([binding_kind], memory_summary_row)?.collect::<Result<_, _>>()?
    } else {
        // LEFT JOIN (not INNER): an UNANCHORED node (#463) has no bindings and must still list —
        // its binding columns come back NULL (rendered blank). The "first binding"
        // correlation moves into the ON clause so a zero-binding memory keeps its row. (The
        // kind-filtered branch above stays an inner join — filtering BY a binding kind
        // correctly excludes bindingless nodes.)
        let mut stmt = conn.prepare(&format!(
            "
            SELECT m.id AS memory_id, m.kind, m.title, m.status,
                   b.binding_kind, b.binding_id
            FROM repo_memories AS m
            LEFT JOIN repo_memory_bindings AS b
                   ON b.memory_id = m.id AND b.repo_id = m.repo_id
              AND b.rowid = (
                  SELECT b2.rowid FROM repo_memory_bindings AS b2
                  WHERE b2.memory_id = m.id AND b2.repo_id = m.repo_id
                  ORDER BY b2.binding_kind, b2.binding_id
                  LIMIT 1
              )
            WHERE m.status IN ('active', 'stale'){repo_clause}
            ORDER BY m.updated_at_ms DESC
            "
        ))?;
        stmt.query_map([], memory_summary_row)?.collect::<Result<_, _>>()?
    };
    Ok(rows)
}

fn memory_summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemorySummary> {
    Ok(MemorySummary {
        memory_id: row.get(0)?,
        kind: row.get(1)?,
        title: row.get(2)?,
        status: row.get(3)?,
        // NULL for an unanchored node (#463, LEFT JOIN) → blank.
        binding_kind: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        binding_id: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
    })
}

/// An active memory whose binding anchor is `gone` or `stale`, together with ranked live
/// candidates that the user can rebind to.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryDoctorEntry {
    pub memory_id: String,
    pub title: String,
    pub binding_kind: String,
    pub binding_id: String,
    pub anchor_status: String,
    /// Qualified names of live same-name symbols, best matches first (kind + signature_hash
    /// agreement ranks higher than bare-name-only hits).
    pub candidates: Vec<String>,
}

/// Read-only report: active memories with `gone | stale` bindings plus live rebind candidates.
///
/// Invariant: this function is purely READ — it never writes to the database.
/// Count of active memories whose anchor is `gone`/`stale` — the EXACT population `doctor_report`
/// lists as ACTIONABLE (`pending` entries are listed there informationally but need no
/// re-anchoring, so they are deliberately not counted here — #492). `scip_moniker` bindings are
/// excluded (self-heal on the next oracle run, never rebind-actionable).
pub fn doctor_attention_count(conn: &Connection) -> anyhow::Result<u64> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "m");
    let count: i64 = conn.query_row(
        &format!(
            "
        SELECT COUNT(*)
        FROM repo_memory_bindings AS b
        JOIN repo_memories AS m ON m.id = b.memory_id AND m.repo_id = b.repo_id
        WHERE m.status = 'active'
          AND b.anchor_status IN ('gone', 'stale')
          AND b.binding_kind != 'scip_moniker'{repo_clause}
        "
        ),
        [],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Distinct memory ids whose anchor is `gone`/`stale` — the SAME population `doctor_report` lists
/// (identical WHERE predicate), exposed as a bare id set so the dream verification queue reuses the
/// doctor predicate instead of re-inlining the anchor-status join. Repo-scoped; `scip_moniker`
/// bindings excluded (self-heal on the next oracle run, never rebind-actionable). Returns
/// `rusqlite::Result` so a `rusqlite::Result` caller (dream) threads it with `?` directly.
pub(crate) fn memory_ids_with_broken_anchors(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let scope = rag_rat_db::schema::periphery_repo_scope(conn, "repo_memories")?;
    let repo_clause = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "m");
    conn.prepare(&format!(
        "SELECT DISTINCT b.memory_id
         FROM repo_memory_bindings AS b
         JOIN repo_memories AS m ON m.id = b.memory_id AND m.repo_id = b.repo_id
         WHERE m.status = 'active'
           AND b.anchor_status IN ('gone', 'stale')
           AND b.binding_kind != 'scip_moniker'{repo_clause}
         ORDER BY b.memory_id"
    ))?
    .query_map([], |row| row.get::<_, String>(0))?
    .collect()
}

pub fn doctor_report(conn: &Connection) -> anyhow::Result<Vec<MemoryDoctorEntry>> {
    // Query bindings whose anchor_status is non-current, restricted to active memories.
    // Mirrors the column list used by validate_memories / binding_row.
    let scope = memory_repo_scope(conn)?;
    let repo_clause = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "m");
    let mut stmt = conn.prepare(&format!(
        "
        SELECT b.memory_id, b.binding_kind, b.binding_id, b.path,
               b.symbol_kind, b.signature_hash, b.anchor_status,
               m.title
        FROM repo_memory_bindings AS b
        JOIN repo_memories AS m ON m.id = b.memory_id AND m.repo_id = b.repo_id
        WHERE m.status = 'active'
          -- `pending` is LISTED (informational: alive on an in-flight branch, #492) but is
          -- deliberately absent from `doctor_attention_count` and the dream queue — it is not
          -- rebind-actionable and must never draw gone-style remediation.
          AND b.anchor_status IN ('gone', 'stale', 'pending')
          -- `scip_moniker` bindings are excluded: a lagging moniker self-heals on the next
          -- `oracle run` and is never rebind-actionable; a genuinely dead symbol surfaces via
          -- its symbol/logical_symbol binding anyway (#70).
          AND b.binding_kind != 'scip_moniker'{repo_clause}
        ORDER BY b.memory_id, b.binding_kind, b.binding_id
        "
    ))?;

    struct RawRow {
        memory_id: String,
        binding_kind: String,
        binding_id: String,
        path: Option<String>,
        symbol_kind: Option<String>,
        signature_hash: Option<String>,
        anchor_status: String,
        title: String,
    }

    let rows = stmt.query_map([], |row| {
        Ok(RawRow {
            memory_id: row.get(0)?,
            binding_kind: row.get(1)?,
            binding_id: row.get(2)?,
            path: row.get(3)?,
            symbol_kind: row.get(4)?,
            signature_hash: row.get(5)?,
            anchor_status: row.get(6)?,
            title: row.get(7)?,
        })
    })?;

    let mut entries = Vec::new();
    for row in rows {
        let r = row?;
        let candidates = live_symbol_candidates(
            conn,
            &r.binding_id,
            r.path.as_deref(),
            r.symbol_kind.as_deref(),
            r.signature_hash.as_deref(),
        );
        entries.push(MemoryDoctorEntry {
            memory_id: r.memory_id,
            title: r.title,
            binding_kind: r.binding_kind,
            binding_id: r.binding_id,
            anchor_status: r.anchor_status,
            candidates,
        });
    }

    // Placeholder-scoped memories (V042): user-authored memories stranded under the
    // '__unassigned__' repo on a consolidated DB — the V042 backfill's leave-at-placeholder path,
    // reachable only by hand-driving `register_repo` before the consolidate importer exists. They
    // are invisible to every scoped memory read, so the doctor must surface them rather than let
    // them vanish silently: one entry per memory under the distinct `placeholder_repo` marker
    // (rebind-by-hand territory — no computable candidates). Skipped when the active repo IS the
    // placeholder (an un-adopted single-repo DB, where placeholder scope is the normal state).
    if let Some(active) = &scope
        && active != rag_rat_base::repo_identity::LEGACY_REPO_ID
    {
        let mut stmt = conn.prepare(
            "
            SELECT id, title FROM repo_memories
            WHERE status IN ('active', 'stale') AND repo_id = ?1
            ORDER BY id
            ",
        )?;
        let rows = stmt.query_map([rag_rat_base::repo_identity::LEGACY_REPO_ID], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (memory_id, title) = row?;
            entries.push(MemoryDoctorEntry {
                memory_id,
                title,
                binding_kind: "repo".to_string(),
                binding_id: rag_rat_base::repo_identity::LEGACY_REPO_ID.to_string(),
                anchor_status: "placeholder_repo".to_string(),
                candidates: Vec::new(),
            });
        }
    }
    Ok(entries)
}

/// Compute live candidate qualified_names for a non-current symbol/logical binding.
/// For path/chunk/edge/commit/tracker bindings there are no computable candidates (empty).
/// Candidates are ranked: same `symbol_kind` AND `signature_hash` match first, then
/// same `symbol_kind` only, then bare-name-only hits last.
fn live_symbol_candidates(
    conn: &Connection,
    binding_id: &str,
    path: Option<&str>,
    stored_kind: Option<&str>,
    stored_sig: Option<&str>,
) -> Vec<String> {
    let short = short_symbol_name(binding_id, path);
    // Run the same bare-name query as relocate_symbol_by_name, but WITHOUT the hash filter —
    // we want all live symbols with this name, ranked by quality, not filtered by content.
    let mut stmt = match conn.prepare(
        "
        SELECT qn.value             AS qualified_name,
               symbols.kind           AS kind,
               symbols.signature      AS signature
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        WHERE symbols.name = ?1
        ORDER BY qn.value
        ",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([short], |row| {
        Ok((
            row.get::<_, String>("qualified_name")?,
            row.get::<_, String>("kind")?,
            row.get::<_, Option<String>>("signature")?,
        ))
    }) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    // Collect with a rank so we can sort: 0 = best (kind+sig), 1 = kind only, 2 = name only.
    let mut ranked: Vec<(u8, String)> = Vec::new();
    for row in rows.flatten() {
        let (qname, kind, signature) = row;
        let sig_hash = signature.map(|s| hex_sha256(s.trim().as_bytes()));
        let kind_match = stored_kind.is_some_and(|k| k == kind);
        let sig_match = stored_sig.is_some() && sig_hash.as_deref() == stored_sig;
        let rank = match (kind_match, sig_match) {
            (true, true) => 0,
            (true, false) => 1,
            _ => 2,
        };
        ranked.push((rank, qname));
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    // Dedupe by qualified_name: cfg-split twins (and overloads) share one qualified_name, so the
    // bare-name query above returns a row per physical symbol. The rebind suggestion is by
    // qualified name, so the duplicates would print the same command twice — collapse them,
    // keeping the best-ranked first occurrence.
    let mut seen = std::collections::HashSet::new();
    ranked
        .into_iter()
        .filter_map(|(_, qname)| seen.insert(qname.clone()).then_some(qname))
        .collect()
}

/// READ-only; counts persisted anchor_status over active memories' bindings, does not re-validate.
/// Returns counts grouped by anchor_status for active repo memories.
pub fn anchor_health_counts(conn: &Connection) -> anyhow::Result<crate::memory::AnchorHealth> {
    let mut health = crate::memory::AnchorHealth::default();
    // Scoped to the active repo (V042): these counts drive per-repo doctor warnings, so a sibling
    // repo's bindings must not inflate them on a consolidated DB.
    let scope = memory_repo_scope(conn)?;
    let repo_clause = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "m");
    let mut stmt = conn.prepare(&format!(
        "
        SELECT b.anchor_status, COUNT(*) AS cnt
        FROM repo_memory_bindings AS b
        JOIN repo_memories AS m ON m.id = b.memory_id AND m.repo_id = b.repo_id
        WHERE m.status = 'active'
          -- Auxiliary `scip_moniker` anchors are excluded exactly as in `doctor_report`: these
          -- counts drive the 'run memory doctor' warnings, so counting rows doctor then hides
          -- would yield an unresolvable warning (#70 review).
          AND b.binding_kind != 'scip_moniker'{repo_clause}
        GROUP BY b.anchor_status
        "
    ))?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
    for row in rows {
        let (status, count) = row?;
        let n = u64::try_from(count).unwrap_or(0);
        match status.as_str() {
            "current" => health.current += n,
            "relocated" => health.relocated += n,
            "stale" => health.stale += n,
            "gone" => health.gone += n,
            _ => {},
        }
    }
    Ok(health)
}

pub fn validate_memories(
    conn: &Connection,
    active_root: Option<&std::path::Path>,
) -> anyhow::Result<RepoMemoryValidationReport> {
    // The off-index filesystem fallback (#98) resolves binding paths against the active checkout
    // root when known (correct under a multi-worktree shared DB), else the persisted source_root.
    let fs_root = effective_fs_root(conn, active_root);
    let fs_root = fs_root.as_deref();
    // Validate ONLY the active repo's bindings (V042): this sweep both counts AND rewrites
    // anchor_status, so an unscoped pass would validate a sibling repo's bindings against THIS
    // repo's index/filesystem — flipping a sibling's healthy anchors to gone (its paths don't
    // exist here) or, worse, blessing a sibling's binding as current when its path collides with
    // one of ours. The downstream UPDATE/DELETE key on rows this SELECT returned, so scoping the
    // read scopes the mutation.
    let scope = memory_repo_scope(conn)?;
    let repo_clause =
        rag_rat_db::schema::periphery_repo_scope_clause(&scope, "repo_memory_bindings");
    // The downgrade-hysteresis torn-window guard (#492): while a STAGED (higher-than-live)
    // generation exists for this repo — a rebuild is mid-flight, or an abandoned staging awaits
    // gc — a `gone` observation may be reading a half-published world, so it must neither ARM
    // nor CONFIRM a downgrade. Positive observations still stamp: evidence of presence is real
    // in any window.
    let staged_window = staged_generation_exists(conn, &scope)?;
    let mut stmt = conn.prepare(&format!(
        "
        SELECT memory_id, binding_kind, binding_id, path, start_line, end_line,
               logical_symbol_id, symbol_id, chunk_id, edge_id, commit_hash, tracker,
               project, item_key, symbol_kind, signature_hash, moniker_tool,
               moniker_tool_version, relocation_reason, anchor_status, created_at_ms,
               downgrade_pending_at_ms
        FROM repo_memory_bindings
        WHERE 1=1{repo_clause}
        ORDER BY CASE WHEN binding_kind = 'scip_moniker' THEN 1 ELSE 0 END,
                 memory_id, binding_kind, binding_id
        "
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok((binding_row(row)?, row.get::<_, Option<i64>>("downgrade_pending_at_ms")?))
    })?;
    // DERIVED write (#560): this pass re-anchors bindings against current source and runs
    // automatically after every index — its output is fully reconstructable, so it stays on the
    // connection's `synchronous = NORMAL` default (no authored-durability bump). Only the authored
    // mutations (create / update / rebind / edge add+remove / op-log genesis) raise FULL.
    let tx = conn.unchecked_transaction()?;
    let mut report = RepoMemoryValidationReport {
        checked: 0,
        current: 0,
        relocated: 0,
        stale: 0,
        gone: 0,
        pending: 0,
        unverified: 0,
    };
    for row in rows {
        let (mut binding, downgrade_pending_at_ms) = row?;
        let original_binding_id = binding.binding_id.clone();
        let stored_status = binding.anchor_status.clone();
        let stored_edge_id = binding.edge_id;
        report.checked += 1;
        let mut status = validate_binding(conn, &mut binding, fs_root)?;
        // Status hysteresis must not retain a stale row id: SQLite can reuse an invalid edge id,
        // and edge-based recall reads this field directly without consulting `anchor_status`.
        // A scoped miss is not enough, however: the same valid id may belong to a linked-worktree
        // edge hidden from this checkout. Preserve that id so one checkout cannot erase another's
        // recall mapping before hysteresis has a chance to reconcile their observations.
        let globally_live_hidden_edge = if status == "gone"
            && binding.binding_kind == "edge"
            && stored_edge_id.is_some()
            && binding.edge_id.is_none()
        {
            match (scope.as_deref(), stored_edge_id) {
                (Some(repo_id), Some(edge_id)) => edge_id_matches_fingerprint_in_linked_worktree(
                    conn,
                    edge_id,
                    repo_id,
                    &original_binding_id,
                )?,
                _ => false,
            }
        } else {
            false
        };
        if globally_live_hidden_edge {
            binding.edge_id = stored_edge_id;
            status = "pending".to_string();
        } else if status == "gone"
            && binding.binding_kind == "edge"
            && stored_edge_id.is_some()
            && binding.edge_id.is_none()
        {
            conn.execute(
                "UPDATE repo_memory_bindings SET edge_id = NULL
                 WHERE memory_id = ?1 AND binding_kind = ?2 AND binding_id = ?3
                   AND (?4 IS NULL OR repo_id = ?4)",
                params![
                    binding.memory_id,
                    binding.binding_kind,
                    original_binding_id,
                    scope.as_deref()
                ],
            )?;
        }
        // Downgrade hysteresis (#492): a single `gone` observation of a not-yet-gone binding is
        // exactly what a torn pass produces (a validate racing a rebuild window, or a sweep from
        // a checkout context that cannot see the anchor another context re-asserts), and a
        // persisted `gone` is what doctor turns into destructive mark-obsolete advice. So a
        // FIRST gone observation only ARMS `downgrade_pending_at_ms` — the stored status stays as
        // it was — and only a SECOND consecutive one persists the downgrade. Invalid edge ids are
        // detached above because identity safety cannot be deferred with status/remediation. Any
        // non-gone stamp clears the marker (the ping-pong never lands), and a staged-generation
        // window freezes the status rule entirely (see `staged_window` above). The REPORT keeps
        // counting the computed observation: what this pass saw is honest; only what doctor reads
        // is hysteresis-guarded.
        if status == "gone" && stored_status != "gone" {
            if staged_window {
                // Untrustworthy observation: leave status and its marker exactly as they were.
            } else if downgrade_pending_at_ms.is_none() {
                conn.execute(
                    "UPDATE repo_memory_bindings SET downgrade_pending_at_ms = ?4
                     WHERE memory_id = ?1 AND binding_kind = ?2 AND binding_id = ?3
                       AND (?5 IS NULL OR repo_id = ?5)",
                    params![
                        binding.memory_id,
                        binding.binding_kind,
                        original_binding_id,
                        now_ms(),
                        scope.as_deref()
                    ],
                )?;
            } else {
                stamp_validated_binding(
                    conn,
                    scope.as_deref(),
                    &binding,
                    &original_binding_id,
                    &status,
                )?;
            }
        } else {
            stamp_validated_binding(
                conn,
                scope.as_deref(),
                &binding,
                &original_binding_id,
                &status,
            )?;
        }
        match status.as_str() {
            "current" => report.current += 1,
            "relocated" => report.relocated += 1,
            "stale" => report.stale += 1,
            "gone" => report.gone += 1,
            "pending" => report.pending += 1,
            _ => report.unverified += 1,
        }
    }
    if report.checked > 0
        && let Some(repo_id) = scope.as_deref()
        && conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM repos WHERE repo_id = ?1)",
            [repo_id],
            |row| row.get::<_, bool>(0),
        )?
    {
        rag_rat_db::meta::bump_lens_revisions(conn, repo_id, &[
            rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
            rag_rat_db::meta::LENS_MEMORIES_REVISION_META,
        ])?;
    }
    tx.commit()?;
    Ok(report)
}

/// Persist one validated binding: the full field rewrite `validate_memories` stamps for every
/// non-deferred observation. Clears `downgrade_pending_at_ms` — a persisted stamp is either an
/// upgrade (the anchor is seen again; the pending downgrade is disarmed) or the confirmed
/// downgrade itself (the marker's job is done).
fn stamp_validated_binding(
    conn: &Connection,
    repo_id: Option<&str>,
    binding: &RepoMemoryBinding,
    original_binding_id: &str,
    status: &str,
) -> anyhow::Result<()> {
    let updated = conn.execute(
        "
        UPDATE OR IGNORE repo_memory_bindings
        SET anchor_status = ?3, logical_symbol_id = ?4, symbol_id = ?5, chunk_id = ?6,
            edge_id = ?7, path = ?8, start_line = ?9, end_line = ?10,
            binding_id = ?11, symbol_kind = ?12, signature_hash = ?13,
            moniker_tool_version = ?15, relocation_reason = ?16,
            downgrade_pending_at_ms = NULL
         WHERE memory_id = ?1 AND binding_kind = ?2 AND binding_id = ?14
           AND (?17 IS NULL OR repo_id = ?17)
        ",
        params![
            binding.memory_id,
            binding.binding_kind,
            status,
            binding.logical_symbol_id,
            binding.symbol_id,
            binding.chunk_id,
            binding.edge_id,
            binding.path,
            binding.start_line,
            binding.end_line,
            binding.binding_id,
            binding.symbol_kind,
            binding.signature_hash,
            original_binding_id,
            binding.moniker_tool_version,
            binding.relocation_reason,
            repo_id
        ],
    )?;
    // UPDATE OR IGNORE: if a sibling binding already holds the new (memory_id, kind,
    // binding_id) PK, the rewrite is a no-op rather than a crash. Drop the
    // now-duplicate stale row.
    if updated == 0 && binding.binding_id != original_binding_id {
        conn.execute(
            "DELETE FROM repo_memory_bindings
             WHERE memory_id = ?1 AND binding_kind = ?2 AND binding_id = ?3
               AND (?4 IS NULL OR repo_id = ?4)",
            params![binding.memory_id, binding.binding_kind, original_binding_id, repo_id],
        )?;
    }
    Ok(())
}

/// Whether a STAGED (higher-than-live) `files` generation exists for the scoped repo (#492): a
/// rebuild is mid-flight, or an abandoned staging awaits gc. Scope `None` (a pre-scoping ladder
/// state) reads as no window — the guard is a refinement, never a gate on validation itself.
fn staged_generation_exists(conn: &Connection, scope: &Option<String>) -> anyhow::Result<bool> {
    let Some(repo_id) = scope.as_deref() else {
        return Ok(false);
    };
    let live = rag_rat_db::schema::live_files_generation(conn, repo_id)?;
    let staged: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.files WHERE repo_id = ?1 AND generation > ?2)",
        params![repo_id, live],
        |row| row.get(0),
    )?;
    Ok(staged)
}
