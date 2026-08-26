use super::*;

pub fn duplicate_memory_id(
    conn: &Connection,
    kind: &str,
    title: &str,
    body: &str,
    payload_json: Option<&str>,
    binding: Option<&ResolvedBinding>,
) -> anyhow::Result<Option<String>> {
    // Dedupe NEVER crosses repos (spec §4.4): a duplicate is a same-repo KIND+title+body+PAYLOAD+
    // binding match — every dimension `memory_input_hash` folds, so two DISTINCT graph-node kinds
    // (a `Concept` and a `Task`) sharing text+payload are NOT duplicates (the second must not be
    // lost). The `{repo_clause}` is empty on the pre-A5 schema (memory still repo-global), so this
    // stays the original global dedupe until the periphery-scoping migration lands. An UNANCHORED
    // node (#463) has no binding, so its dupe is a same-repo title+body+payload match with NO
    // bindings — never a false collision with an anchored memory that happens to share text. The
    // `payload_json IS ?` compare is NULL-safe (both-null OR equal), so two polymorphic nodes
    // (#465) with identical text but DIFFERENT payloads are NOT duplicates — neither collapses
    // onto the other (which would silently drop the second's payload).
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    match binding {
        Some(binding) => conn.query_row(
            &format!(
                "
        SELECT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
         AND repo_memory_bindings.repo_id = repo_memories.repo_id
        WHERE repo_memories.kind = ?6
          AND lower(repo_memories.title) = lower(?1)
          AND lower(repo_memories.body) = lower(?2)
          AND repo_memory_bindings.binding_kind = ?3
          AND repo_memory_bindings.binding_id = ?4
          AND repo_memories.payload_json IS ?5
          AND repo_memories.status != 'obsolete'{repo_clause}
        LIMIT 1
        "
            ),
            params![
                title.trim(),
                body.trim(),
                binding.binding_kind,
                binding.binding_id,
                payload_json,
                kind
            ],
            |row| row.get("memory_id"),
        ),
        None => conn.query_row(
            &format!(
                "
        SELECT repo_memories.id AS memory_id
        FROM repo_memories
        WHERE repo_memories.kind = ?4
          AND lower(repo_memories.title) = lower(?1)
          AND lower(repo_memories.body) = lower(?2)
          AND repo_memories.payload_json IS ?3
          AND repo_memories.status != 'obsolete'{repo_clause}
           AND NOT EXISTS (
              SELECT 1 FROM repo_memory_bindings
              WHERE repo_memory_bindings.memory_id = repo_memories.id
                AND repo_memory_bindings.repo_id = repo_memories.repo_id
           )
        LIMIT 1
        "
            ),
            params![title.trim(), body.trim(), payload_json, kind],
            |row| row.get("memory_id"),
        ),
    }
    .optional()
    .map_err(Into::into)
}
/// The canonical tag-SET form — trimmed, empties dropped, deduped, sorted. [`replace_tags`] STORES
/// this shape and [`tags_for_memory`] reads it back sorted, so comparing `normalize_tags(input)`
/// against a stored set is a stable "did the tags actually change?" test (a whitespace- or
/// duplicate-only difference is NOT a change — the op-log update-detection relies on this).
pub fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> =
        tags.iter().map(|tag| tag.trim().to_string()).filter(|tag| !tag.is_empty()).collect();
    out.sort();
    out.dedup();
    out
}

pub fn replace_tags(conn: &Connection, memory_id: &str, tags: &[String]) -> anyhow::Result<()> {
    conn.execute("DELETE FROM repo_memory_tags WHERE memory_id = ?1", [memory_id])?;
    for tag in normalize_tags(tags) {
        validate_len("tag", &tag, 64)?;
        conn.execute(
            "INSERT OR IGNORE INTO repo_memory_tags(memory_id, tag) VALUES (?1, ?2)",
            params![memory_id, tag],
        )?;
    }
    Ok(())
}
pub fn upsert_memory_fts(conn: &Connection, memory_id: &str) -> anyhow::Result<()> {
    conn.execute("DELETE FROM repo_memory_fts WHERE memory_id = ?1", [memory_id])?;
    let tags = tags_for_memory(conn, memory_id)?.join(" ");
    // Post-A5 the FTS carries a `repo_id UNINDEXED` mirror of the parent memory's, so
    // `memory_search` can filter it after the MATCH. Stamp it by COPYING `repo_memories.repo_id`
    // (already set by the time this runs). The branch keys on the SCHEMA capability (does the
    // column exist?), NOT on the connection's active-repo scope: a scope-less writer on the current
    // schema (e.g. the synced-content drain at open, before `set_context`) must still stamp the
    // repo_id it copies from the row, or `memory_search`'s repo filter would never match the row
    // and a subsequent no-op write would never repair it. On the pre-A5 schema the column does not
    // exist, so write the original row shape.
    if rag_rat_db::schema::column_exists(conn, "repo_memories", "repo_id")? {
        conn.execute(
            "
            INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
            SELECT repo_id, id, title, body, kind, ?2
            FROM repo_memories
            WHERE id = ?1
            ",
            params![memory_id, tags],
        )?;
    } else {
        conn.execute(
            "
            INSERT INTO repo_memory_fts(memory_id, title, body, kind, tags)
            SELECT id, title, body, kind, ?2
            FROM repo_memories
            WHERE id = ?1
            ",
            params![memory_id, tags],
        )?;
    }
    Ok(())
}
/// #582: recover `repo_memory_fts` after shadow-table corruption. Lossless first: FTS5
/// `'rebuild'` re-derives the inverted index from the table's own content shadow. When the
/// content shadow is torn too (the `'rebuild'` itself errors), fall back to the nuclear path —
/// DROP + CREATE + repopulate from `repo_memories` (the FTS is derived; the memories table is
/// the source of truth, so nothing is lost). The nuclear shape carries `repo_id` (post-A5);
/// a pre-A5 store has no source `repo_id` to rebuild from, so it gets the lossless path only.
pub fn heal_repo_memory_fts(conn: &Connection) -> anyhow::Result<()> {
    if conn.execute("INSERT INTO repo_memory_fts(repo_memory_fts) VALUES('rebuild')", []).is_ok() {
        return Ok(());
    }
    anyhow::ensure!(
        memory_repo_scope(conn)?.is_some(),
        "repo_memory_fts is corrupt beyond an in-place rebuild and this pre-A5 store cannot be \
         repopulated from source"
    );
    rag_rat_db::schema::rebuild_repo_memory_fts_with_repo_id(conn)?;
    Ok(())
}

pub(crate) fn memory_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoMemory> {
    Ok(RepoMemory {
        memory_id: row.get("memory_id")?,
        kind: row.get("kind")?,
        title: row.get("title")?,
        body: row.get("body")?,
        // Populated only by the summary-first surface (see `apply_memory_surface`); the mechanical
        // hydration always yields the full body.
        summary: None,
        verdict: None,
        confidence: row.get("confidence")?,
        status: row.get("status")?,
        created_by: row.get("created_by")?,
        created_at_ms: row.get("created_at_ms")?,
        updated_at_ms: row.get("updated_at_ms")?,
        source: row.get("source")?,
        payload_json: row.get("payload_json")?,
        source_text_hash: row.get("source_text_hash")?,
        input_hash: row.get("input_hash")?,
        memory_version: row.get("memory_version")?,
        // Drift is a property of the reading surface, not of the row: only the drive-by readers
        // mark it (`mark_drifted_synced_anchor`). The mechanical hydration leaves it clear so
        // `memory_get`, `memory_search`, dream, distill and doctor are unaffected.
        synced_anchor_drifted: false,
        bindings: Vec::new(),
        call_paths: Vec::new(),
        tags: Vec::new(),
    })
}
pub(crate) fn binding_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoMemoryBinding> {
    Ok(RepoMemoryBinding {
        memory_id: row.get("memory_id")?,
        binding_kind: row.get("binding_kind")?,
        binding_id: row.get("binding_id")?,
        path: row.get("path")?,
        start_line: row.get("start_line")?,
        end_line: row.get("end_line")?,
        logical_symbol_id: row.get("logical_symbol_id")?,
        symbol_id: row.get("symbol_id")?,
        chunk_id: row.get("chunk_id")?,
        edge_id: row.get("edge_id")?,
        commit_hash: row.get("commit_hash")?,
        tracker: row.get("tracker")?,
        project: row.get("project")?,
        item_key: row.get("item_key")?,
        symbol_kind: row.get("symbol_kind")?,
        signature_hash: row.get("signature_hash")?,
        moniker_tool: row.get("moniker_tool")?,
        moniker_tool_version: row.get("moniker_tool_version")?,
        relocation_reason: row.get("relocation_reason")?,
        anchor_status: row.get("anchor_status")?,
        created_at_ms: row.get("created_at_ms")?,
    })
}
pub(crate) fn attach_memory_children(
    conn: &Connection,
    memory: &mut RepoMemory,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(
        "
        SELECT memory_id, binding_kind, binding_id, path, start_line, end_line, logical_symbol_id,
               symbol_id, chunk_id, edge_id, commit_hash, tracker, project,
               item_key, symbol_kind, signature_hash, moniker_tool, moniker_tool_version,
               relocation_reason, anchor_status, created_at_ms
        FROM repo_memory_bindings
        WHERE memory_id = ?1
          AND repo_id = (SELECT repo_id FROM repo_memories WHERE id = ?1)
        ORDER BY binding_kind, binding_id
        ",
    )?;
    memory.bindings =
        stmt.query_map([&memory.memory_id], binding_row)?.collect::<Result<Vec<_>, _>>()?;
    let mut stmt = conn.prepare(
        "
        SELECT memory_id, start_logical_symbol_id, end_logical_symbol_id, edge_sequence_hash,
               path_summary, created_at_ms
        FROM repo_memory_call_paths
        WHERE memory_id = ?1
        ORDER BY created_at_ms, edge_sequence_hash
        ",
    )?;
    memory.call_paths =
        stmt.query_map([&memory.memory_id], call_path_row)?.collect::<Result<Vec<_>, _>>()?;
    memory.tags = tags_for_memory(conn, &memory.memory_id)?;
    Ok(())
}
pub(crate) fn call_path_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoMemoryCallPath> {
    Ok(RepoMemoryCallPath {
        memory_id: row.get("memory_id")?,
        start_logical_symbol_id: row.get("start_logical_symbol_id")?,
        end_logical_symbol_id: row.get("end_logical_symbol_id")?,
        edge_sequence_hash: row.get("edge_sequence_hash")?,
        path_summary: row.get("path_summary")?,
        created_at_ms: row.get("created_at_ms")?,
    })
}
pub fn tags_for_memory(conn: &Connection, memory_id: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT tag FROM repo_memory_tags WHERE memory_id = ?1 ORDER BY tag")?;
    stmt.query_map([memory_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}
/// Mark a synced memory whose anchored text this checkout no longer holds (#1236).
///
/// The stamp is not a bespoke hash: `resolve.rs` takes it from `chunks.text_hash` for a
/// chunk/symbol anchor and from `files.sha256` for an edge anchor, so the current value is the
/// same column read back — a join, not a recomputation, and no filesystem read on a drive-by
/// surface. Comparing anything else (a fresh span read, or `files.sha256` for a chunk anchor)
/// would compare against a quantity the author never stamped.
///
/// Drift is "the stamp matches nothing this checkout currently holds at the memory's anchors": the
/// memory carries candidate hashes and none of them equals the stamp. A content-confirmed
/// relocation therefore passes by construction — it moved the anchor to text that hashes the same.
/// A moniker- or name-matched relocation onto changed text does mark, which is the honest reading
/// of a pure hash comparison and costs a mark rather than a disappearance.
pub(crate) fn mark_drifted_synced_anchor(
    conn: &Connection,
    memory: &mut RepoMemory,
) -> anyhow::Result<()> {
    // An absent stamp is not evidence of drift — every pre-carrier row is NULL.
    let Some(stamp) = memory.source_text_hash.as_deref() else { return Ok(()) };
    // Scoped to synced rows. A local memory's stamp is its own authoring snapshot, and local
    // drift already has a mechanism: relocation stamps `anchor_status`, which demotes on its own.
    let synced: bool = conn.query_row(
        "SELECT origin = 'synced' FROM repo_memories WHERE id = ?1",
        [&memory.memory_id],
        |row| row.get(0),
    )?;
    if !synced {
        return Ok(());
    }
    // Every anchor is priced by the SAME quantity its resolver stamped: `chunks.text_hash` for a
    // chunk/symbol anchor, `files.sha256` for an edge anchor, and `files.sha256` by path for a
    // `path` anchor (spanned or bare — `resolve_path_binding` stamps both from the file). Omitting
    // the path branch would leave a peer's path-anchored memory permanently unpriced, and so
    // permanently presented as current however far its file had moved on.
    //
    // The last two branches carry the SEEDED state. `seed_node_anchors` writes portable columns
    // only and leaves `chunk_id`/`edge_id` at their defaults for the validate/relocate loop to
    // fill, and nothing runs that loop automatically after a drain — so keying solely on the
    // resolved ids would leave every freshly synced symbol and edge anchor unpriced for as long as
    // no one happened to run `memory_validate`, which is exactly the window in which a peer's
    // memories first appear. Both are reachable from the portable identity: an edge anchor's
    // `path` IS its source file's, and a symbol anchor's span selects the chunk covering it.
    //
    // Reads go through the SCOPED `files` view, never `main.files`. The view is what this checkout
    // actually serves: it applies the live generation, drops tombstones, keeps a sibling
    // checkout's rows out, and — the part a hand-rolled predicate gets wrong — SHADOWS a base row
    // whose path a linked worktree overrides. Selecting from `main.files` retains both rows, so a
    // stamp matching the hidden base hash would read as current while the checkout serves changed
    // overlay text.
    let (candidates, matches): (i64, i64) = conn.query_row(
        "
        SELECT COUNT(*), COALESCE(SUM(current_hash = ?2), 0)
        FROM (
            SELECT chunks.text_hash AS current_hash
            FROM repo_memory_bindings
            JOIN chunks ON chunks.id = repo_memory_bindings.chunk_id
            JOIN files ON files.id = chunks.file_id
            WHERE repo_memory_bindings.memory_id = ?1
            UNION ALL
            SELECT files.sha256 AS current_hash
            FROM repo_memory_bindings
            JOIN edges ON edges.id = repo_memory_bindings.edge_id
            JOIN files ON files.id = edges.source_file_id
            WHERE repo_memory_bindings.memory_id = ?1
            UNION ALL
            SELECT files.sha256 AS current_hash
            FROM repo_memory_bindings
            JOIN files ON files.path = repo_memory_bindings.path
            WHERE repo_memory_bindings.memory_id = ?1
              AND repo_memory_bindings.binding_kind = 'path'
            UNION ALL
            SELECT files.sha256 AS current_hash
            FROM repo_memory_bindings
            JOIN files ON files.path = repo_memory_bindings.path
            WHERE repo_memory_bindings.memory_id = ?1
              AND repo_memory_bindings.binding_kind = 'edge'
              AND repo_memory_bindings.edge_id IS NULL
            UNION ALL
            SELECT chunks.text_hash AS current_hash
            FROM repo_memory_bindings
            JOIN files ON files.path = repo_memory_bindings.path
            JOIN chunks ON chunks.file_id = files.id
                       AND chunks.start_line <= repo_memory_bindings.start_line
                       AND chunks.end_line >= repo_memory_bindings.end_line
            WHERE repo_memory_bindings.memory_id = ?1
              AND repo_memory_bindings.binding_kind IN ('symbol', 'logical_symbol')
              AND repo_memory_bindings.chunk_id IS NULL
              AND repo_memory_bindings.start_line IS NOT NULL
              AND repo_memory_bindings.end_line IS NOT NULL
        )
        ",
        params![&memory.memory_id, stamp],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    // An anchor this checkout cannot price at all — its file shadowed, tombstoned, or never
    // indexed here — leaves the memory unmarked: absence of evidence, not evidence of drift.
    memory.synced_anchor_drifted = candidates > 0 && matches == 0;
    Ok(())
}

/// Hydrate ids into memories for a DRIVE-BY surface — the five `memories_for_*` readers. Unlike
/// bare `memory_by_id`, this marks anchor drift (#1236), which is why the drive-by readers must
/// route through here rather than looping `memory_by_id` themselves.
pub(crate) fn drive_by_memory(
    conn: &Connection,
    memory_id: &str,
) -> anyhow::Result<Option<RepoMemory>> {
    let Some(mut memory) = memory_by_id(conn, memory_id)? else { return Ok(None) };
    mark_drifted_synced_anchor(conn, &mut memory)?;
    Ok(Some(memory))
}

pub(crate) fn ids_to_memories(
    conn: &Connection,
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<String>>,
) -> anyhow::Result<Vec<RepoMemory>> {
    let mut memories = Vec::new();
    for row in rows {
        if let Some(memory) = drive_by_memory(conn, &row?)? {
            memories.push(memory);
        }
    }
    Ok(memories)
}

#[derive(Debug, Default)]
pub struct CurrentDreamState {
    pub summary: Option<String>,
    pub verdict: Option<String>,
    pub direction: Option<String>,
    pub evidence_json: Option<String>,
    pub checked_against_commit: Option<String>,
}

/// Raw dream state for the memory's current note and evidence inputs. Unlike the compact renderer,
/// this preserves the individual verdict fields for structured consumers.
pub fn current_dream_state(
    conn: &Connection,
    memory_id: &str,
    title: &str,
    body: &str,
) -> rusqlite::Result<CurrentDreamState> {
    use rag_rat_db::schema;

    let content_hash = crate::memory::evidence::note_content_hash(title, body);
    let scope = schema::periphery_repo_scope(conn, "memory_summaries")?;
    let summary_clause = schema::periphery_repo_scope_clause(&scope, "memory_summaries");
    let summary = conn
        .query_row(
            &format!(
                "SELECT summary FROM memory_summaries WHERE memory_id = ?1 AND content_hash = ?2 \
                 AND prompt_version = ?3{summary_clause}"
            ),
            params![memory_id, content_hash, crate::memory::evidence::COMPACT_PROMPT_VERSION],
            |row| row.get(0),
        )
        .optional()?;
    let reality_clause = schema::periphery_repo_scope_clause(&scope, "memory_reality");
    let reality = conn
        .query_row(
            &format!(
                "SELECT verdict, direction, evidence_json, checked_against_commit, \
                        checked_inputs_hash
                 FROM memory_reality
                 WHERE memory_id = ?1 AND content_hash = ?2 AND prompt_version = \
                       ?3{reality_clause}"
            ),
            params![memory_id, content_hash, crate::memory::evidence::VERDICT_PROMPT_VERSION],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((verdict, direction, evidence_json, checked_against_commit, stored_inputs)) = reality
    else {
        return Ok(CurrentDreamState { summary, ..CurrentDreamState::default() });
    };
    let current_inputs = crate::memory::evidence::checked_inputs_hash(conn, memory_id, &scope)?;
    if stored_inputs.as_deref() != Some(current_inputs.as_str()) {
        return Ok(CurrentDreamState { summary, ..CurrentDreamState::default() });
    }
    Ok(CurrentDreamState { summary, verdict, direction, evidence_json, checked_against_commit })
}

/// The dream summary + verdict marker for a memory's CURRENT note (title+body) — the `[memory]
/// surface = "summary"` hydration (dream v2 passes 1 & 2). Returns `(summary, verdict_marker)`:
///   - `summary` is the `memory_summaries.summary` keyed on the memory's current `content_hash`
///     (repo-scoped); a title OR body edit changes the key, so a stale summary self-invalidates and
///     this misses (title-only fallback) until the compaction pass regenerates it.
///   - `verdict_marker` is a plain-text marker derived from the memory's `memory_reality` verdict
///     for the CURRENT note (`[verdict: diverged]` / `[verdict: current @<short-commit>]`), keyed
///     on `content_hash` exactly like the summary: a title or body edit changes the key, so a stale
///     verdict self-invalidates and this misses until the next verdict pass re-checks. `None` when
///     there is no matching verdict row or the row's verdict is still NULL (a pass-0-only check).
///
/// Reads only the derived sibling tables — never a `repo_memories` column.
pub fn current_summary_and_verdict(
    conn: &Connection,
    memory_id: &str,
    title: &str,
    body: &str,
) -> rusqlite::Result<(Option<String>, Option<String>)> {
    let state = current_dream_state(conn, memory_id, title, body)?;
    let marker =
        render_verdict_marker(state.verdict.as_deref(), state.checked_against_commit.as_deref());
    Ok((state.summary, marker))
}

/// A plain-text drive-by verdict marker (no emoji, matching the mechanical rendering style). A
/// `current` verdict carries the short (7-hex) commit it was checked against when known; a
/// `diverged` verdict stands alone. A NULL/unrecognized verdict (a pass-0-only reality row, before
/// the model ran) has no marker.
fn render_verdict_marker(verdict: Option<&str>, commit: Option<&str>) -> Option<String> {
    match verdict {
        Some("current") => Some(match commit.map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) => format!("[verdict: current @{}]", c.chars().take(7).collect::<String>()),
            None => "[verdict: current]".to_string(),
        }),
        Some("diverged") => Some("[verdict: diverged]".to_string()),
        _ => None,
    }
}

pub fn split_active_stale(memories: Vec<RepoMemory>) -> (Vec<RepoMemory>, Vec<RepoMemory>) {
    let mut direct = Vec::new();
    let mut stale = Vec::new();
    for memory in memories {
        // The auxiliary `scip_moniker` binding never demotes a memory: it is an identity anchor
        // for relocation (#70), not a content anchor, and it naturally lags between (opt-in)
        // oracle runs. A real problem with the anchored code shows on the primary
        // symbol/logical_symbol binding, which still demotes.
        if memory.status == "stale"
            // A synced memory anchored to text this checkout no longer holds (#1236). It still
            // surfaces — in the demoted lane, like any other weakened anchor — because a hash
            // divergence cannot distinguish a peer running ahead from a local edit after a pull.
            || memory.synced_anchor_drifted
            || memory.bindings.iter().any(|binding| {
                binding.binding_kind != SCIP_MONIKER_BINDING_KIND
                    && matches!(
                        binding.anchor_status.as_str(),
                        // `pending` (#492) joins the demoted bucket: the anchored code is not in
                        // THIS context, so drive-by evidence must not present as confidently
                        // current — but unlike `gone` it draws no remediation.
                        "stale" | "gone" | "unverified" | "pending"
                    )
            })
        {
            stale.push(memory);
        } else {
            direct.push(memory);
        }
    }
    (direct, stale)
}

#[cfg(test)]
mod drift_tests {
    use super::*;
    use crate::memory::api::{memories_for_chunk, memories_for_path, memories_for_symbol};

    const REPO: &str = "r";

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::MigrationHooks::noop()).unwrap();
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [REPO],
        )
        .unwrap();
        conn
    }

    /// A file with one chunk whose current text hashes to `text_hash`.
    fn seed_chunk(conn: &Connection, path: &str, text_hash: &str) -> i64 {
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, repo_id, generation) VALUES \
             (?1,'rust','source',?2,0,0,'','',?3,0)",
            params![path, format!("sha-{path}"), REPO],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
             text_hash) VALUES (?1,'code',0,10,1,5,?2)",
            params![file_id, text_hash],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// A memory carrying `stamp` as its author-stamped hash, anchored to `chunk_id`.
    fn seed_memory(conn: &Connection, id: &str, origin: &str, stamp: Option<&str>, chunk_id: i64) {
        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version, repo_id, origin, \
             source_text_hash) VALUES \
             (?1,'Invariant','t','b','high','active','agent',1,1,'agent','v1',?2,?3,?4)",
            params![id, REPO, origin, stamp],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             chunk_id, anchor_status, created_at_ms, repo_id) VALUES \
             (?1,'chunk',?2,'src/a.rs',?3,'current',0,?4)",
            params![id, chunk_id.to_string(), chunk_id, REPO],
        )
        .unwrap();
    }

    #[test]
    fn a_synced_memory_whose_anchor_text_moved_on_is_demoted_but_still_surfaces() {
        let conn = db();
        // The checkout now holds `now`; the author stamped `then`.
        let chunk = seed_chunk(&conn, "src/a.rs", "now");
        install_files_view(&conn, "");
        seed_memory(&conn, "m1", "synced", Some("then"), chunk);

        let found = memories_for_chunk(&conn, chunk, 10).unwrap();
        assert_eq!(found.len(), 1, "drift must never hide a memory");
        assert!(found[0].synced_anchor_drifted, "a diverged stamp marks");

        let (direct, stale) = split_active_stale(found);
        assert!(direct.is_empty(), "a drifted anchor does not present as confidently current");
        assert_eq!(stale.len(), 1, "it is demoted into the stale lane, not dropped");
    }

    #[test]
    fn a_synced_memory_still_anchored_to_its_stamped_text_is_untouched() {
        let conn = db();
        // What a content-confirmed relocation leaves behind: the anchor moved, the text did not,
        // so the stamp still names what is there.
        let chunk = seed_chunk(&conn, "src/a.rs", "same");
        install_files_view(&conn, "");
        seed_memory(&conn, "m1", "synced", Some("same"), chunk);

        let found = memories_for_chunk(&conn, chunk, 10).unwrap();
        assert!(!found[0].synced_anchor_drifted, "a matching stamp is not drift");
        assert_eq!(split_active_stale(found).0.len(), 1, "and it stays in the direct lane");
    }

    #[test]
    fn a_synced_memory_carrying_no_stamp_surfaces_unmarked() {
        let conn = db();
        // Every pre-carrier row is NULL. Absence of a stamp is not evidence of drift.
        let chunk = seed_chunk(&conn, "src/a.rs", "now");
        install_files_view(&conn, "");
        seed_memory(&conn, "m1", "synced", None, chunk);

        let found = memories_for_chunk(&conn, chunk, 10).unwrap();
        assert!(!found[0].synced_anchor_drifted, "a NULL stamp cannot diverge");
        assert_eq!(split_active_stale(found).0.len(), 1);
    }

    #[test]
    fn a_local_memory_in_identical_drift_is_not_marked() {
        let conn = db();
        // Same divergence as the marked case, authored locally. Local drift is relocation's job;
        // marking it here would demote most of a living repo's own memories.
        let chunk = seed_chunk(&conn, "src/a.rs", "now");
        install_files_view(&conn, "");
        seed_memory(&conn, "m1", "local", Some("then"), chunk);

        let found = memories_for_chunk(&conn, chunk, 10).unwrap();
        assert!(!found[0].synced_anchor_drifted, "the rule is scoped to synced rows");
        assert_eq!(split_active_stale(found).0.len(), 1);
    }

    #[test]
    fn a_path_anchor_is_priced_by_its_files_hash() {
        let conn = db();
        // `resolve_path_binding` stamps a path anchor from `files.sha256`, so this checkout can
        // price it — and must. Leaving path anchors out of the candidate set would keep a peer's
        // path-anchored memory presenting as current however far its file had moved on.
        seed_chunk(&conn, "src/a.rs", "irrelevant");
        install_files_view(&conn, "");
        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version, repo_id, origin, \
             source_text_hash) VALUES \
             ('m1','Invariant','t','b','high','active','agent',1,1,'agent','v1',?1,'synced','\
             stamped-then')",
            params![REPO],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             start_line, end_line, anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/a.rs:1-5','src/a.rs',1,5,'current',0,?1)",
            params![REPO],
        )
        .unwrap();

        let found = memories_for_path(&conn, "src/a.rs", 10).unwrap();
        assert_eq!(found.len(), 1, "still surfaces");
        assert!(found[0].synced_anchor_drifted, "the file's hash is not what the author stamped");
    }

    #[test]
    fn an_anchor_this_checkout_does_not_serve_leaves_the_memory_unmarked() {
        let conn = db();
        // The anchored file is not in this checkout's view at all, so nothing here can speak to
        // the memory either way. Absence of evidence is not evidence of drift.
        let chunk = seed_chunk(&conn, "src/a.rs", "now");
        seed_memory(&conn, "m1", "synced", Some("then"), chunk);
        conn.execute_batch(
            "DROP VIEW IF EXISTS temp.files;
             CREATE TEMP VIEW temp.files AS SELECT * FROM main.files WHERE 0",
        )
        .unwrap();

        let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
        mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
        assert!(!memory.synced_anchor_drifted, "an unserved anchor yields no verdict");
    }

    #[test]
    fn an_overlay_shadows_the_base_row_it_overrides() {
        let conn = db();
        // A linked worktree overrides the path. The scoped view hides the base row, so the stamp
        // must be judged against the OVERLAY text this checkout serves — matching the hidden base
        // hash is exactly the false "current" a `main.files` read would produce.
        seed_chunk_in(&conn, "src/a.rs", "base-text", "");
        conn.execute("UPDATE main.files SET sha256 = 'stamped-base' WHERE worktree_id = ''", [])
            .unwrap();
        seed_chunk_in(&conn, "src/a.rs", "overlay-text", "wt-active");
        conn.execute(
            "UPDATE main.files SET sha256 = 'moved-on' WHERE worktree_id = 'wt-active'",
            [],
        )
        .unwrap();
        set_active_worktree(&conn, "wt-active");
        install_files_view(&conn, "wt-active");

        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version, repo_id, origin, \
             source_text_hash) VALUES \
             ('m1','Invariant','t','b','high','active','agent',1,1,'agent','v1',?1,'synced','\
             stamped-base')",
            params![REPO],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/a.rs','src/a.rs','current',0,?1)",
            params![REPO],
        )
        .unwrap();

        let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
        mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
        assert!(
            memory.synced_anchor_drifted,
            "the shadowed base hash must not certify a memory as current"
        );
    }

    #[test]
    fn every_drive_by_reader_marks_including_the_one_that_hydrates_its_own_ids() {
        let conn = db();
        let chunk = seed_chunk(&conn, "src/a.rs", "now");
        install_files_view(&conn, "");
        seed_memory(&conn, "m1", "synced", Some("then"), chunk);
        // `memories_for_symbol` collects ids into a set and hydrates them itself rather than going
        // through `ids_to_memories`, so it is the reader a seam-only fix would silently miss.
        conn.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','path','src/a.rs','src/a.rs','current',0,?1)",
            params![REPO],
        )
        .unwrap();

        let hit = crate::symbol::SymbolHit {
            symbol_id: 0,
            logical_symbol_id: None,
            logical_variant_count: None,
            logical_group_reason: None,
            file_id: 0,
            path: "src/a.rs".to_string(),
            file_kind: "source".to_string(),
            language: "rust".to_string(),
            name: "a".to_string(),
            symbol_path: "src/a.rs::a".to_string(),
            qualified_name: "src/a.rs::a".to_string(),
            kind: "function".to_string(),
            start_byte: 0,
            end_byte: 0,
            signature: None,
            docs: None,
            importance: None,
        };
        let found = memories_for_symbol(&conn, &hit, 10).unwrap();
        assert_eq!(found.len(), 1, "the symbol reader still finds it");
        assert!(found[0].synced_anchor_drifted, "and marks it, like the other four readers");
    }

    /// Seed a file+chunk owned by a specific checkout, so the active-scope filter can be exercised.
    fn seed_chunk_in(conn: &Connection, path: &str, text_hash: &str, worktree: &str) -> i64 {
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, repo_id, generation) VALUES \
             (?1,'rust','source',?2,0,0,'',?3,?4,0)",
            params![path, format!("sha-{path}"), worktree, REPO],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
             text_hash) VALUES (?1,'code',0,10,1,5,?2)",
            params![file_id, text_hash],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn set_active_worktree(conn: &Connection, worktree: &str) {
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('worktree_id', ?1)",
            [worktree],
        )
        .unwrap();
    }

    /// The scoped `files` view a real connection carries, in the shape `lifecycle.rs` installs:
    /// the active checkout's own rows, plus base rows for paths that checkout does not override.
    /// Reads under test must see exactly what this checkout serves, shadowing included.
    fn install_files_view(conn: &Connection, active_worktree: &str) {
        conn.execute_batch(&format!(
            "DROP VIEW IF EXISTS temp.files;
             CREATE TEMP VIEW temp.files AS
             SELECT * FROM main.files
              WHERE repo_id = '{REPO}' AND generation = 0 AND kind != 'deleted'
                AND worktree_id = '{active_worktree}' AND worktree_id != ''
             UNION ALL
             SELECT * FROM main.files
              WHERE repo_id = '{REPO}' AND generation = 0 AND kind != 'deleted'
                AND worktree_id = ''
                AND path NOT IN (
                    SELECT path FROM main.files
                     WHERE repo_id = '{REPO}' AND generation = 0 AND kind != 'deleted'
                       AND worktree_id = '{active_worktree}' AND worktree_id != ''
                )"
        ))
        .unwrap();
    }

    #[test]
    fn a_sibling_checkouts_chunk_never_prices_this_checkouts_anchor() {
        let conn = db();
        // The anchor names a chunk owned by ANOTHER checkout's file row — the shape a binding
        // takes when it was resolved over there. That text is not what this checkout serves, so
        // it must not decide this checkout's verdict; the memory simply goes unpriced here.
        let theirs = seed_chunk_in(&conn, "src/a.rs", "their-text", "wt-sibling");
        set_active_worktree(&conn, "wt-active");
        install_files_view(&conn, "wt-active");
        seed_memory(&conn, "m1", "synced", Some("stamped"), theirs);

        let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
        mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
        assert!(
            !memory.synced_anchor_drifted,
            "a sibling checkout's text is not evidence about this one"
        );
    }

    #[test]
    fn the_shared_base_row_still_prices_an_anchor_inside_a_linked_worktree() {
        let conn = db();
        // The `worktree_id = ''` base row belongs to every checkout, so working inside a linked
        // worktree must not silence drift on files that checkout has not overridden — otherwise
        // the filter above would turn every linked worktree into a blanket exemption.
        let base = seed_chunk_in(&conn, "src/a.rs", "now", "");
        set_active_worktree(&conn, "wt-active");
        install_files_view(&conn, "wt-active");
        seed_memory(&conn, "m1", "synced", Some("then"), base);

        let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
        mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
        assert!(memory.synced_anchor_drifted, "the shared base row is this checkout's text too");
    }

    #[test]
    fn a_superseded_generation_row_never_prices_an_anchor() {
        let conn = db();
        // A staging row from an in-flight rebuild carries a higher generation than the live one.
        // It is not what any reader is served, so it must not answer for the anchor either.
        let chunk = seed_chunk(&conn, "src/a.rs", "now");
        conn.execute("UPDATE main.files SET generation = 7 WHERE path = 'src/a.rs'", []).unwrap();
        install_files_view(&conn, "");
        seed_memory(&conn, "m1", "synced", Some("then"), chunk);

        let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
        mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
        assert!(
            !memory.synced_anchor_drifted,
            "no LIVE row prices this anchor, so there is no verdict to reach"
        );
    }

    #[test]
    fn a_deleted_files_chunk_never_prices_an_anchor() {
        let conn = db();
        let chunk = seed_chunk(&conn, "src/a.rs", "now");
        conn.execute("UPDATE main.files SET kind = 'deleted' WHERE path = 'src/a.rs'", []).unwrap();
        install_files_view(&conn, "");
        seed_memory(&conn, "m1", "synced", Some("then"), chunk);

        let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
        mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
        assert!(!memory.synced_anchor_drifted, "a tombstone is not evidence of drift");
    }

    #[test]
    fn an_edge_anchor_is_priced_by_its_source_files_hash() {
        let conn = db();
        // An edge anchor is stamped from the source file's `sha256` (`edge_by_id`), so that is
        // what prices it. Without this branch a peer's edge-anchored memory would go unpriced and
        // present as current no matter how far the file holding the call had moved on.
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, repo_id, generation) VALUES \
             ('src/a.rs','rust','source','moved-on',0,0,'','',?1,0)",
            params![REPO],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO edges_data(source_file_id, to_name_id, resolution_id, edge_kind_id, \
             confidence_id) VALUES (?1, 0, 0, 0, 0)",
            params![file_id],
        )
        .unwrap();
        let edge_id = conn.last_insert_rowid();
        install_files_view(&conn, "");
        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version, repo_id, origin, \
             source_text_hash) VALUES \
             ('m1','Invariant','t','b','high','active','agent',1,1,'agent','v1',?1,'synced','\
             stamped-then')",
            params![REPO],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, edge_id, \
             anchor_status, created_at_ms, repo_id) VALUES \
             ('m1','edge','fp','src/a.rs',?1,'current',0,?2)",
            params![edge_id, REPO],
        )
        .unwrap();

        let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
        mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
        assert!(memory.synced_anchor_drifted, "the edge's source file is not what was stamped");
    }

    #[test]
    fn a_drifted_memory_carries_the_flag_on_the_wire() {
        let conn = db();
        // The augmenters and the raw MCP readers return a list and never partition it, so the
        // demotion has to survive serialization or those surfaces present a drifted memory as
        // plainly current. An undrifted memory adds no field.
        let chunk = seed_chunk(&conn, "src/a.rs", "now");
        install_files_view(&conn, "");
        seed_memory(&conn, "m1", "synced", Some("then"), chunk);

        let found = memories_for_chunk(&conn, chunk, 10).unwrap();
        let json = serde_json::to_value(&found[0]).unwrap();
        assert_eq!(
            json.get("synced_anchor_drifted").and_then(serde_json::Value::as_bool),
            Some(true),
            "a consumer that never calls split_active_stale still sees the divergence"
        );

        let conn2 = db();
        let ok = seed_chunk(&conn2, "src/a.rs", "same");
        install_files_view(&conn2, "");
        seed_memory(&conn2, "m2", "synced", Some("same"), ok);
        let clean = memories_for_chunk(&conn2, ok, 10).unwrap();
        assert!(
            serde_json::to_value(&clean[0]).unwrap().get("synced_anchor_drifted").is_none(),
            "the common case stays off the wire"
        );
    }

    /// The row shape `seed_node_anchors` writes: portable columns only, resolved ids left NULL.
    fn seed_unresolved_binding(
        conn: &Connection,
        kind: &str,
        path: &str,
        span: Option<(i64, i64)>,
    ) {
        conn.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             start_line, end_line, anchor_status, created_at_ms, repo_id) VALUES \
             ('m1',?1,'portable-id',?2,?3,?4,'unverified',0,?5)",
            params![kind, path, span.map(|s| s.0), span.map(|s| s.1), REPO],
        )
        .unwrap();
    }

    fn seed_bare_memory(conn: &Connection, stamp: &str) {
        conn.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version, repo_id, origin, \
             source_text_hash) VALUES \
             ('m1','Invariant','t','b','high','active','agent',1,1,'agent','v1',?1,'synced',?2)",
            params![REPO, stamp],
        )
        .unwrap();
    }

    #[test]
    fn a_freshly_seeded_symbol_anchor_is_priced_before_validation_resolves_it() {
        let conn = db();
        // Straight out of the drain: the seeder writes portable columns only, and nothing runs the
        // validate/relocate loop for it automatically. Keying on `chunk_id` alone would leave a
        // peer's symbol-anchored memory unpriced for as long as nobody ran `memory_validate` —
        // precisely the window in which their memories first show up.
        seed_chunk(&conn, "src/a.rs", "now");
        install_files_view(&conn, "");
        seed_bare_memory(&conn, "stamped-then");
        seed_unresolved_binding(&conn, "logical_symbol", "src/a.rs", Some((2, 4)));

        let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
        mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
        assert!(memory.synced_anchor_drifted, "the covering chunk prices an unresolved symbol");
    }

    #[test]
    fn a_freshly_seeded_symbol_anchor_still_matching_is_not_marked() {
        let conn = db();
        // The mirror: pricing the seeded state must not manufacture drift for a peer whose text
        // this checkout genuinely still holds.
        seed_chunk(&conn, "src/a.rs", "same");
        install_files_view(&conn, "");
        seed_bare_memory(&conn, "same");
        seed_unresolved_binding(&conn, "logical_symbol", "src/a.rs", Some((2, 4)));

        let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
        mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
        assert!(!memory.synced_anchor_drifted, "a seeded anchor on matching text is current");
    }

    #[test]
    fn a_freshly_seeded_edge_anchor_is_priced_by_its_source_file() {
        let conn = db();
        // An edge anchor's stamp is its source file's hash, and the seeded row already carries
        // that file's path — so the portable identity prices it exactly, with no id to resolve.
        conn.execute(
            "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, repo_id, generation) VALUES \
             ('src/a.rs','rust','source','moved-on',0,0,'','',?1,0)",
            params![REPO],
        )
        .unwrap();
        install_files_view(&conn, "");
        seed_bare_memory(&conn, "stamped-then");
        seed_unresolved_binding(&conn, "edge", "src/a.rs", None);

        let mut memory = memory_by_id(&conn, "m1").unwrap().unwrap();
        mark_drifted_synced_anchor(&conn, &mut memory).unwrap();
        assert!(memory.synced_anchor_drifted, "an unresolved edge is priced by its file");
    }

    #[test]
    fn a_by_id_read_never_marks() {
        let conn = db();
        let chunk = seed_chunk(&conn, "src/a.rs", "now");
        install_files_view(&conn, "");
        seed_memory(&conn, "m1", "synced", Some("then"), chunk);
        // `memory_by_id` backs `memory_get` and `memory_search`. Drift is a drive-by presentation
        // rule, so those surfaces must be untouched by it.
        let direct = memory_by_id(&conn, "m1").unwrap().unwrap();
        assert!(!direct.synced_anchor_drifted, "memory_get / memory_search stay unaffected");
    }
}
