use super::*;

pub(crate) fn duplicate_memory_id(
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
              SELECT 1 FROM repo_memory_bindings WHERE repo_memory_bindings.memory_id = \
                 repo_memories.id
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
pub(crate) fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> =
        tags.iter().map(|tag| tag.trim().to_string()).filter(|tag| !tag.is_empty()).collect();
    out.sort();
    out.dedup();
    out
}

pub(crate) fn replace_tags(
    conn: &Connection,
    memory_id: &str,
    tags: &[String],
) -> anyhow::Result<()> {
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
pub(crate) fn upsert_memory_fts(conn: &Connection, memory_id: &str) -> anyhow::Result<()> {
    conn.execute("DELETE FROM repo_memory_fts WHERE memory_id = ?1", [memory_id])?;
    let tags = tags_for_memory(conn, memory_id)?.join(" ");
    // Post-A5 the FTS carries a `repo_id UNINDEXED` mirror of the parent memory's, so
    // `memory_search` can filter it after the MATCH. Stamp it from `repo_memories.repo_id`
    // (already set by the time this runs). On the pre-A5 schema the column does not exist, so
    // write the original row shape.
    if memory_repo_scope(conn)?.is_some() {
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
pub(crate) fn heal_repo_memory_fts(conn: &Connection) -> anyhow::Result<()> {
    if conn.execute("INSERT INTO repo_memory_fts(repo_memory_fts) VALUES('rebuild')", []).is_ok() {
        return Ok(());
    }
    anyhow::ensure!(
        memory_repo_scope(conn)?.is_some(),
        "repo_memory_fts is corrupt beyond an in-place rebuild and this pre-A5 store cannot be \
         repopulated from source"
    );
    crate::index::schema::rebuild_repo_memory_fts_with_repo_id(conn)?;
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
pub(crate) fn tags_for_memory(conn: &Connection, memory_id: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT tag FROM repo_memory_tags WHERE memory_id = ?1 ORDER BY tag")?;
    stmt.query_map([memory_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}
pub(crate) fn ids_to_memories(
    conn: &Connection,
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<String>>,
) -> anyhow::Result<Vec<RepoMemory>> {
    let mut memories = Vec::new();
    for row in rows {
        if let Some(memory) = memory_by_id(conn, &row?)? {
            memories.push(memory);
        }
    }
    Ok(memories)
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
pub(crate) fn current_summary_and_verdict(
    conn: &Connection,
    memory_id: &str,
    title: &str,
    body: &str,
) -> rusqlite::Result<(Option<String>, Option<String>)> {
    use crate::index::schema;
    // The dream freshness key is over the WHOLE note (title+body) — recompute it exactly as the
    // queue / verdict pass / compaction pass stamp it.
    let content_hash = crate::dream::note_content_hash(title, body);
    // Scope both sibling reads by the active repo (both carry `repo_id`, V045). One probe suffices
    // — they scope to the same active repo id.
    let scope = schema::periphery_repo_scope(conn, "memory_summaries")?;
    let summary_clause = schema::periphery_repo_scope_clause(&scope, "memory_summaries");
    // Gate the summary on the SAME identity the compaction queue treats as "covered": current
    // content_hash AND current `COMPACT_PROMPT_VERSION`. Without the version predicate a summary
    // produced by an obsolete compact prompt/guards keeps surfacing while the memory waits
    // behind the compaction budget (or a model failure) for a fresh one.
    let summary: Option<String> = conn
        .query_row(
            &format!(
                "SELECT summary FROM memory_summaries WHERE memory_id = ?1 AND content_hash = ?2 \
                 AND prompt_version = ?3{summary_clause}"
            ),
            params![memory_id, content_hash, crate::dream::COMPACT_PROMPT_VERSION],
            |r| r.get(0),
        )
        .optional()?;
    let reality_clause = schema::periphery_repo_scope_clause(&scope, "memory_reality");
    // Gate the verdict marker on the current verdict `PROMPT_VERSION` too (the SQL predicate), so a
    // verdict from an obsolete prompt is not shown while the queue waits to re-check it. The
    // remaining stale checks — content_hash (the WHERE) and the evidence-pack hash (below) — mirror
    // the queue and divergence finder exactly.
    let reality: Option<(Option<String>, Option<String>, Option<String>)> = conn
        .query_row(
            &format!(
                "SELECT verdict, checked_against_commit, checked_inputs_hash FROM memory_reality \
                 WHERE memory_id = ?1 AND content_hash = ?2 AND prompt_version = \
                 ?3{reality_clause}"
            ),
            params![memory_id, content_hash, crate::dream::VERDICT_PROMPT_VERSION],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    // Gate the marker on the evidence-pack hash the SAME way the queue and divergence finder do: a
    // verdict is only shown when the memory's current evidence (bound files + identifier
    // resolutions) still matches what it was checked against, so an evidence change (before a
    // re-verify) drops the marker instead of showing a verdict checked against prior code. The
    // hash is recomputed ONLY when a body- and prompt-matching verdict row exists (the rare
    // case), so surfacing an unverified memory pays nothing.
    let verdict_marker = match reality {
        Some((verdict, commit, stored_inputs)) => {
            let current_inputs = crate::dream::checked_inputs_hash(conn, memory_id, &scope)?;
            if stored_inputs.as_deref() == Some(current_inputs.as_str()) {
                render_verdict_marker(verdict.as_deref(), commit.as_deref())
            } else {
                None
            }
        },
        None => None,
    };
    Ok((summary, verdict_marker))
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

pub(crate) fn split_active_stale(memories: Vec<RepoMemory>) -> (Vec<RepoMemory>, Vec<RepoMemory>) {
    let mut direct = Vec::new();
    let mut stale = Vec::new();
    for memory in memories {
        // The auxiliary `scip_moniker` binding never demotes a memory: it is an identity anchor
        // for relocation (#70), not a content anchor, and it naturally lags between (opt-in)
        // oracle runs. A real problem with the anchored code shows on the primary
        // symbol/logical_symbol binding, which still demotes.
        if memory.status == "stale"
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
