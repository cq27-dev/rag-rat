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
          AND {BINDING_CURRENT_BINDING_ID} = ?4
          AND repo_memories.payload_json IS ?5
          AND repo_memories.status != 'obsolete'{repo_clause}
        LIMIT 1
        "
            ),
            params![
                title.trim(),
                body.trim(),
                binding.binding_kind.as_db_str(),
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
/// The SELECT list every reader that hydrates through [`binding_row`] must carry: the authored
/// columns and this store's `resolved_*` shadows (#1297).
pub(crate) const BINDING_ROW_COLUMNS: &str =
    "memory_id, binding_kind, binding_id, path, start_line, end_line, logical_symbol_id, \
     symbol_id, chunk_id, edge_id, commit_hash, tracker, project, item_key, symbol_kind, \
     signature_hash, moniker_tool, moniker_tool_version, relocation_reason, anchor_status, \
     created_at_ms, resolved, resolved_binding_id, resolved_path, resolved_start_line, \
     resolved_end_line, resolved_symbol_kind, resolved_signature_hash, \
     resolved_moniker_tool_version";

pub(crate) fn binding_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoMemoryBinding> {
    // The location and discriminator fields are this store's resolution where it has one —
    // `resolved` set, and then the shadows are its view, NULL included — else the authored value;
    // `binding_id` stays the authored identity (#1297).
    let resolved = row.get::<_, Option<i64>>("resolved")?.is_some_and(|flag| flag != 0);
    fn pick<T: rusqlite::types::FromSql>(
        row: &rusqlite::Row<'_>,
        resolved: bool,
        shadow: &str,
        authored: &str,
    ) -> rusqlite::Result<Option<T>> {
        row.get(if resolved { shadow } else { authored })
    }
    let binding_id: String = row.get("binding_id")?;
    let resolved_binding_id = if resolved {
        row.get::<_, Option<String>>("resolved_binding_id")?.filter(|id| *id != binding_id)
    } else {
        None
    };
    Ok(RepoMemoryBinding {
        memory_id: row.get("memory_id")?,
        binding_kind: row.get("binding_kind")?,
        binding_id,
        resolved_binding_id,
        path: pick(row, resolved, "resolved_path", "path")?,
        start_line: pick(row, resolved, "resolved_start_line", "start_line")?,
        end_line: pick(row, resolved, "resolved_end_line", "end_line")?,
        logical_symbol_id: row.get("logical_symbol_id")?,
        symbol_id: row.get("symbol_id")?,
        chunk_id: row.get("chunk_id")?,
        edge_id: row.get("edge_id")?,
        commit_hash: row.get("commit_hash")?,
        tracker: row.get("tracker")?,
        project: row.get("project")?,
        item_key: row.get("item_key")?,
        symbol_kind: pick(row, resolved, "resolved_symbol_kind", "symbol_kind")?,
        signature_hash: pick(row, resolved, "resolved_signature_hash", "signature_hash")?,
        moniker_tool: row.get("moniker_tool")?,
        moniker_tool_version: pick(
            row,
            resolved,
            "resolved_moniker_tool_version",
            "moniker_tool_version",
        )?,
        relocation_reason: row.get("relocation_reason")?,
        anchor_status: row.get("anchor_status")?,
        created_at_ms: row.get("created_at_ms")?,
    })
}
pub(crate) fn attach_memory_children(
    conn: &Connection,
    memory: &mut RepoMemory,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(&format!(
        "
        SELECT {BINDING_ROW_COLUMNS}
        FROM repo_memory_bindings
        WHERE memory_id = ?1
          AND repo_id = (SELECT repo_id FROM repo_memories WHERE id = ?1)
        ORDER BY binding_kind, binding_id
        ",
    ))?;
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
    // The fallback keys on whether the resolved id is SERVED here, not on whether it is set. A
    // validated binding inside a linked worktree keeps pointing at the base row the overlay
    // shadows, so an id that is present but invisible is no more usable than a missing one —
    // gating on `IS NULL` would leave that memory unpriced while the checkout serves changed text.
    //
    // Reads go through the SCOPED `files` view, never `main.files`. The view is what this checkout
    // actually serves: it applies the live generation, drops tombstones, keeps a sibling
    // checkout's rows out, and — the part a hand-rolled predicate gets wrong — SHADOWS a base row
    // whose path a linked worktree overrides. Selecting from `main.files` retains both rows, so a
    // stamp matching the hidden base hash would read as current while the checkout serves changed
    // overlay text.
    let current_start_line = binding_current("repo_memory_bindings", "start_line");
    let current_end_line = binding_current("repo_memory_bindings", "end_line");
    let (candidates, matches): (i64, i64) = conn.query_row(
        &format!(
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
            JOIN files ON files.path = {BINDING_CURRENT_PATH}
            WHERE repo_memory_bindings.memory_id = ?1
              AND repo_memory_bindings.binding_kind = 'path'
            UNION ALL
            SELECT files.sha256 AS current_hash
            FROM repo_memory_bindings
            JOIN files ON files.path = {BINDING_CURRENT_PATH}
            WHERE repo_memory_bindings.memory_id = ?1
              AND repo_memory_bindings.binding_kind = 'edge'
              AND NOT EXISTS (
                  SELECT 1 FROM edges
                  JOIN files AS served ON served.id = edges.source_file_id
                  WHERE edges.id = repo_memory_bindings.edge_id
              )
            UNION ALL
            SELECT chunks.text_hash AS current_hash
            FROM repo_memory_bindings
            JOIN files ON files.path = {BINDING_CURRENT_PATH}
            JOIN chunks ON chunks.file_id = files.id
                       AND chunks.start_line <= {current_start_line}
                       AND chunks.end_line >= {current_end_line}
            WHERE repo_memory_bindings.memory_id = ?1
              AND repo_memory_bindings.binding_kind IN ('symbol', 'logical_symbol')
              AND NOT EXISTS (
                  SELECT 1 FROM chunks AS resolved
                  JOIN files AS served ON served.id = resolved.file_id
                  WHERE resolved.id = repo_memory_bindings.chunk_id
              )
              AND {current_start_line} IS NOT NULL
              AND {current_end_line} IS NOT NULL
        )
        "
        ),
        params![&memory.memory_id, stamp],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    // An anchor this checkout cannot price at all — its file shadowed, tombstoned, or never
    // indexed here — leaves the memory unmarked: absence of evidence, not evidence of drift.
    memory.synced_anchor_drifted = candidates > 0 && matches == 0;
    Ok(())
}

/// Mark anchor drift across a list a drive-by surface is about to render (#1236).
///
/// The augmenters assemble one list from several lanes — symbol, path, and a lexical lane fed by
/// `memory_search_scored`, which hydrates through plain `memory_by_id`. Rendering that list without
/// this leaves the same memory marked or unmarked depending on which lane happened to find it,
/// which is worse than not marking at all: the reader cannot tell a current anchor from an
/// unpriced one. Idempotent, so lanes already marked at hydration recompute the same answer.
pub fn mark_drive_by_drift(conn: &Connection, memories: &mut [RepoMemory]) -> anyhow::Result<()> {
    for memory in memories.iter_mut() {
        mark_drifted_synced_anchor(conn, memory)?;
    }
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
    let scope = schema::periphery_repo_scope(conn, "memory_note_summaries")?;
    let summary_clause = schema::periphery_repo_scope_clause(&scope, "memory_note_summaries");
    let summary = conn
        .query_row(
            &format!(
                "SELECT summary FROM memory_note_summaries WHERE memory_id = ?1 AND content_hash \
                 = ?2 AND prompt_version = ?3{summary_clause}"
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
///   - `summary` is the `memory_note_summaries.summary` whose `content_hash` is the memory's
///     current one (repo-scoped); a title OR body edit changes that hash, so the stored row no
///     longer matches and this misses (title-only fallback) until the compaction pass regenerates
///     it.
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
                binding.binding_kind != BindingKind::ScipMoniker.as_db_str()
                    && matches!(
                        AnchorStatus::from_db_str(&binding.anchor_status).ok(),
                        // `pending` (#492) joins the demoted bucket: the anchored code is not in
                        // THIS context, so drive-by evidence must not present as confidently
                        // current — but unlike `gone` it draws no remediation.
                        Some(
                            AnchorStatus::Stale
                                | AnchorStatus::Gone
                                | AnchorStatus::Unverified
                                | AnchorStatus::Pending
                        )
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
#[path = "hydrate/drift_tests.rs"]
mod drift_tests;
