use super::*;

pub(crate) fn duplicate_memory_id(
    conn: &Connection,
    title: &str,
    body: &str,
    binding: &ResolvedBinding,
) -> anyhow::Result<Option<String>> {
    // Dedupe NEVER crosses repos (spec §4.4): a duplicate is a same-repo title+body+binding match.
    // The `{repo_clause}` is empty on the pre-A5 schema (memory still repo-global), so this stays
    // the original global dedupe until the periphery-scoping migration lands.
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    conn.query_row(
        &format!(
            "
        SELECT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
        WHERE lower(repo_memories.title) = lower(?1)
          AND lower(repo_memories.body) = lower(?2)
          AND repo_memory_bindings.binding_kind = ?3
          AND repo_memory_bindings.binding_id = ?4
          AND repo_memories.status != 'obsolete'{repo_clause}
        LIMIT 1
        "
        ),
        params![title.trim(), body.trim(), binding.binding_kind, binding.binding_id],
        |row| row.get("memory_id"),
    )
    .optional()
    .map_err(Into::into)
}
pub(crate) fn replace_tags(
    conn: &Connection,
    memory_id: &str,
    tags: &[String],
) -> anyhow::Result<()> {
    conn.execute("DELETE FROM repo_memory_tags WHERE memory_id = ?1", [memory_id])?;
    for tag in tags.iter().map(|tag| tag.trim()).filter(|tag| !tag.is_empty()) {
        validate_len("tag", tag, 64)?;
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
pub(crate) fn memory_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoMemory> {
    Ok(RepoMemory {
        memory_id: row.get("memory_id")?,
        kind: row.get("kind")?,
        title: row.get("title")?,
        body: row.get("body")?,
        confidence: row.get("confidence")?,
        status: row.get("status")?,
        created_by: row.get("created_by")?,
        created_at_ms: row.get("created_at_ms")?,
        updated_at_ms: row.get("updated_at_ms")?,
        source: row.get("source")?,
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
        github_owner: row.get("github_owner")?,
        github_repo: row.get("github_repo")?,
        github_number: row.get("github_number")?,
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
               symbol_id, chunk_id, edge_id, commit_hash, github_owner, github_repo,
               github_number, symbol_kind, signature_hash, moniker_tool, moniker_tool_version,
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
                    && matches!(binding.anchor_status.as_str(), "stale" | "gone" | "unverified")
            })
        {
            stale.push(memory);
        } else {
            direct.push(memory);
        }
    }
    (direct, stale)
}
