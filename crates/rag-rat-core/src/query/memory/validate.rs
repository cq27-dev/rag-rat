use super::*;

pub(crate) fn validate_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    match binding.binding_kind.as_str() {
        "logical_symbol" => validate_logical_symbol_binding(conn, binding),
        "symbol" => validate_symbol_binding(conn, binding),
        "chunk" => validate_chunk_binding(conn, binding),
        "edge" => validate_edge_binding(conn, binding),
        "call_path" => validate_call_path_binding(conn, binding),
        "scip_moniker" => validate_moniker_binding(conn, binding),
        "path" => validate_path_binding(conn, binding),
        "dir" => validate_dir_binding(conn, binding),
        "commit" | "github" => Ok("unverified".to_string()),
        _ => Ok("unverified".to_string()),
    }
}
/// Validate a `dir` binding: current while at least one indexed file lives at or under the
/// directory, gone otherwise. Dir bindings are descriptive anchors with no `source_text_hash`
/// — they never go stale, only current or gone.
pub(crate) fn validate_dir_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    let dir = binding.path.clone().unwrap_or_else(|| binding.binding_id.clone());
    Ok(if dir_has_files(conn, &dir)? { "current" } else { "gone" }.to_string())
}
pub(crate) fn validate_logical_symbol_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    if let Some(id) = binding.logical_symbol_id
        && crate::query::symbol::lookup_logical_by_id(conn, id)?.is_some()
    {
        return validate_bound_chunk(conn, binding);
    }
    let relocated = conn
        .query_row(
            "
            SELECT id, path
            FROM logical_symbols
            WHERE qualified_name = ?1
            LIMIT 1
            ",
            [&binding.binding_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((id, path)) = relocated {
        binding.logical_symbol_id = Some(id);
        binding.path = Some(path);
        if let Some(chunk) = chunk_for_logical_symbol(conn, id)? {
            binding.symbol_id = chunk.symbol_id;
            binding.chunk_id = Some(chunk.chunk_id);
            binding.start_line = Some(chunk.start_line);
            binding.end_line = Some(chunk.end_line);
        }
        return Ok("relocated".to_string());
    }
    // Cross-file move: bare name + content hash fallback (same path as symbol binding).
    if let Some(hash) = source_hash_for_memory(conn, &binding.memory_id)? {
        let short = short_symbol_name(&binding.binding_id, binding.path.as_deref()).to_string();
        if let Some(m) = relocate_symbol_by_name(conn, &short, &hash)? {
            // binding_id becomes the relocated member symbol's qualified_name, not a
            // logical_symbols.qualified_name. The stable logical_symbol_id arm re-matches on the
            // next reindex; if that ever goes stale this bare-name fallback recovers it — the
            // logical-qualified-name arm above intentionally won't re-match this binding again.
            binding.binding_id = m.binding_id;
            binding.logical_symbol_id = m.logical_symbol_id;
            binding.symbol_id = Some(m.symbol_id);
            binding.path = Some(m.path);
            binding.chunk_id = m.chunk_id;
            binding.start_line = m.start_line;
            binding.end_line = m.end_line;
            binding.symbol_kind = m.symbol_kind;
            binding.signature_hash = m.signature_hash;
            return Ok("relocated".to_string());
        }
    }
    relocate_via_moniker_or_gone(conn, binding)
}
pub(crate) fn validate_symbol_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    if let Some(id) = binding.symbol_id
        && crate::query::symbol::lookup_by_id(conn, id)?.is_some()
    {
        return validate_bound_chunk(conn, binding);
    }
    let relocated = conn
        .query_row(
            "
            SELECT symbols.id, files.path
            FROM symbols
            JOIN files ON files.id = symbols.file_id
            WHERE symbols.qualified_name = ?1
            LIMIT 1
            ",
            [&binding.binding_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((id, path)) = relocated {
        binding.symbol_id = Some(id);
        binding.logical_symbol_id = logical_symbol_id_for_symbol(conn, id)?;
        binding.path = Some(path);
        if let Some(chunk) = chunk_for_symbol(conn, id, &binding.binding_id)? {
            binding.chunk_id = Some(chunk.chunk_id);
            binding.start_line = Some(chunk.start_line);
            binding.end_line = Some(chunk.end_line);
        }
        let (kind, sig) = symbol_signal(conn, id)?;
        binding.symbol_kind = kind;
        binding.signature_hash = sig;
        return Ok("relocated".to_string());
    }
    // Cross-file move: qualified_name changed with the path. Match by bare name + content hash.
    if let Some(hash) = source_hash_for_memory(conn, &binding.memory_id)? {
        let short = short_symbol_name(&binding.binding_id, binding.path.as_deref()).to_string();
        if let Some(m) = relocate_symbol_by_name(conn, &short, &hash)? {
            binding.binding_id = m.binding_id;
            binding.symbol_id = Some(m.symbol_id);
            binding.logical_symbol_id = m.logical_symbol_id;
            binding.path = Some(m.path);
            binding.chunk_id = m.chunk_id;
            binding.start_line = m.start_line;
            binding.end_line = m.end_line;
            binding.symbol_kind = m.symbol_kind;
            binding.signature_hash = m.signature_hash;
            return Ok("relocated".to_string());
        }
    }
    relocate_via_moniker_or_gone(conn, binding)
}

/// Last-resort relocation for a symbol/logical_symbol binding whose qualified-name and
/// name+content-hash anchors are exhausted: re-resolve the memory's recorded SCIP moniker against
/// current oracle data (#70). A unique live match — semantic identity, robust to content edits the
/// hash fallback can't survive — relocates with `relocation_reason = "moniker-match"`; otherwise
/// the binding is gone.
fn relocate_via_moniker_or_gone(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    if let Some(m) = relocate_binding_by_moniker(conn, binding)? {
        binding.binding_id = m.binding_id;
        binding.symbol_id = Some(m.symbol_id);
        binding.logical_symbol_id = m.logical_symbol_id;
        binding.path = Some(m.path);
        binding.chunk_id = m.chunk_id;
        binding.start_line = m.start_line;
        binding.end_line = m.end_line;
        binding.symbol_kind = m.symbol_kind;
        binding.signature_hash = m.signature_hash;
        binding.relocation_reason = Some(MONIKER_MATCH_REASON.to_string());
        return Ok("relocated".to_string());
    }
    Ok("gone".to_string())
}
pub(crate) fn validate_chunk_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    let status = validate_bound_chunk(conn, binding)?;
    if status != "gone" {
        return Ok(status);
    }
    let Some(hash) = source_hash_for_memory(conn, &binding.memory_id)? else {
        return Ok("gone".to_string());
    };
    let Some(chunk) = relocate_chunk_by_hash(conn, &hash)? else {
        return Ok("gone".to_string());
    };
    binding.binding_id = chunk.chunk_id.to_string();
    binding.chunk_id = Some(chunk.chunk_id);
    binding.path = Some(chunk.path);
    binding.start_line = Some(chunk.start_line);
    binding.end_line = Some(chunk.end_line);
    Ok("relocated".to_string())
}
pub(crate) fn validate_edge_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    if let Some(edge_id) = binding.edge_id
        && let Some(edge) = edge_by_id(conn, edge_id)?
    {
        binding.path = Some(edge.path);
        binding.start_line = Some(edge.start_line);
        binding.end_line = Some(edge.end_line);
        binding.symbol_id = None;
        binding.logical_symbol_id = None;
        return validate_bound_edge_source_hash(conn, binding, &edge.source_hash);
    }
    let Some(edge) = edge_by_fingerprint(conn, &binding.binding_id)? else {
        return Ok("gone".to_string());
    };
    binding.edge_id = Some(edge.edge_id);
    binding.path = Some(edge.path);
    binding.start_line = Some(edge.start_line);
    binding.end_line = Some(edge.end_line);
    binding.symbol_id = None;
    binding.logical_symbol_id = None;
    Ok("relocated".to_string())
}
pub(crate) fn validate_call_path_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    // Re-check each stored edge behind the server-derived hash (#38). Exact-fingerprint match →
    // the edge is unchanged; loose name/kind/target match → it moved lines (relocated); neither →
    // that edge is gone.
    let mut stmt = conn.prepare(
        "
        SELECT edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name
        FROM repo_memory_call_path_edges
        WHERE memory_id = ?1 AND edge_sequence_hash = ?2
        ORDER BY ordinal
        ",
    )?;
    let edges = stmt
        .query_map(params![binding.memory_id, binding.binding_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if edges.is_empty() {
        // Legacy client-supplied hash with no stored edges — honest-but-weak: current only as a
        // row, never verifiable against the graph.
        let exists = conn.query_row(
            "SELECT COUNT(*) FROM repo_memory_call_paths
             WHERE memory_id = ?1 AND edge_sequence_hash = ?2",
            params![binding.memory_id, binding.binding_id],
            |row| row.get::<_, i64>(0),
        )?;
        return Ok(if exists > 0 { "unverified" } else { "gone" }.to_string());
    }

    let total = edges.len();
    let mut relocated = 0usize;
    let mut gone = 0usize;
    for (fingerprint, from_name, to_name, edge_kind, target) in &edges {
        if edge_by_fingerprint(conn, fingerprint)?.is_some() {
            continue;
        }
        if call_path_edge_relocatable(
            conn,
            from_name.as_deref(),
            to_name.as_deref(),
            edge_kind,
            target.as_deref(),
        )? {
            relocated += 1;
        } else {
            gone += 1;
        }
    }

    Ok(if gone == total {
        "gone"
    } else if gone > 0 {
        "stale"
    } else if relocated > 0 {
        "relocated"
    } else {
        "current"
    }
    .to_string())
}

/// Is there still an edge matching this one's loose identity (names/kind/target), ignoring line
/// numbers? Used to call a call-path edge `relocated` (moved) rather than `gone` (#38).
fn call_path_edge_relocatable(
    conn: &Connection,
    from_name: Option<&str>,
    to_name: Option<&str>,
    edge_kind: &str,
    target_qualified_name: Option<&str>,
) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row(
        "
        SELECT COUNT(*)
        FROM edges
        WHERE edge_kind = ?3
          AND COALESCE(from_name, '') = COALESCE(?1, '')
          AND COALESCE(to_name, '') = COALESCE(?2, '')
          AND COALESCE(target_qualified_name, '') = COALESCE(?4, '')
        ",
        params![from_name, to_name, edge_kind, target_qualified_name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}
pub(crate) fn validate_bound_edge_source_hash(
    conn: &Connection,
    binding: &RepoMemoryBinding,
    current_source_hash: &str,
) -> anyhow::Result<String> {
    match source_hash_for_memory(conn, &binding.memory_id)? {
        Some(expected) if expected != current_source_hash => Ok("stale".to_string()),
        _ => Ok("current".to_string()),
    }
}
pub(crate) fn validate_bound_chunk(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    let Some(chunk_id) = binding.chunk_id else {
        return Ok("unverified".to_string());
    };
    let Some(chunk) = chunk_by_id(conn, chunk_id)? else {
        return Ok("gone".to_string());
    };
    binding.path = Some(chunk.path);
    binding.start_line = Some(chunk.start_line);
    binding.end_line = Some(chunk.end_line);
    match source_hash_for_memory(conn, &binding.memory_id)? {
        Some(expected) if expected != chunk.text_hash => Ok("stale".to_string()),
        _ => Ok("current".to_string()),
    }
}
pub(crate) fn validate_path_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    let Some(path) = binding.path.as_deref() else {
        return Ok("unverified".to_string());
    };
    let current_hash = conn
        .query_row(
            "SELECT sha256 FROM files WHERE path = ?1 ORDER BY id DESC LIMIT 1",
            [path],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(current_hash) = current_hash else {
        return Ok("gone".to_string());
    };
    // A BARE path binding (no line span) is an AREA anchor, like a `dir` binding: the claim is
    // "this note is about this file", not "this file's bytes are X" — so it is current while the
    // file is indexed, never content-stale. Hashing the whole file made every commit stale every
    // area-level note bound to a touched file, permanently (nothing refreshes the hash), which
    // buried the real staleness signals under noise. Only a SPANNED `path:start-end` binding
    // claims specific content and keeps the content-hash check.
    if binding.start_line.is_none() && binding.end_line.is_none() {
        return Ok("current".to_string());
    }
    match source_hash_for_memory(conn, &binding.memory_id)? {
        Some(expected) if expected != current_hash => Ok("stale".to_string()),
        _ => Ok("current".to_string()),
    }
}
pub(crate) fn source_hash_for_memory(
    conn: &Connection,
    memory_id: &str,
) -> anyhow::Result<Option<String>> {
    conn.query_row("SELECT source_text_hash FROM repo_memories WHERE id = ?1", [memory_id], |row| {
        row.get::<_, Option<String>>(0)
    })
    .optional()
    .map(|value| value.flatten())
    .map_err(Into::into)
}
pub(crate) fn validate_kind(kind: &str) -> anyhow::Result<()> {
    match kind {
        "Invariant"
        | "Decision"
        | "RejectedAlternative"
        | "Risk"
        | "BugPattern"
        | "TestExpectation"
        | "PerformanceNote"
        | "SecurityNote"
        | "FFIBoundary"
        | "PlatformQuirk"
        | "FollowUp"
        | "OpenQuestion"
        | "Obsolete" => Ok(()),
        _ => anyhow::bail!("invalid memory kind `{kind}`"),
    }
}
pub(crate) fn validate_confidence(confidence: &str) -> anyhow::Result<()> {
    match confidence {
        "high" | "medium" | "low" => Ok(()),
        _ => anyhow::bail!("invalid memory confidence `{confidence}`"),
    }
}
pub(crate) fn validate_status(status: &str) -> anyhow::Result<()> {
    match status {
        "active" | "stale" | "obsolete" | "rejected" => Ok(()),
        _ => anyhow::bail!("invalid memory status `{status}`"),
    }
}
pub(crate) fn validate_source(source: &str) -> anyhow::Result<()> {
    match source {
        "agent" | "human" | "imported" | "generated" => Ok(()),
        _ => anyhow::bail!("invalid memory source `{source}`"),
    }
}
pub(crate) fn validate_len(field: &str, value: &str, max: usize) -> anyhow::Result<()> {
    let len = value.trim().chars().count();
    if len == 0 {
        anyhow::bail!("memory {field} must not be empty");
    }
    if len > max {
        anyhow::bail!("memory {field} exceeds {max} characters");
    }
    Ok(())
}
pub(crate) fn memory_id(now: i64, input_hash: &str) -> String {
    let suffix = input_hash.chars().take(12).collect::<String>();
    format!("mem_{now:x}_{suffix}")
}
pub(crate) fn memory_input_hash(kind: &str, title: &str, body: &str, tags: &[String]) -> String {
    let mut normalized_tags = tags.iter().map(|tag| tag.trim()).collect::<Vec<_>>();
    normalized_tags.sort_unstable();
    hex_sha256(
        format!("{kind}\n{}\n{}\n{}", title.trim(), body.trim(), normalized_tags.join(","))
            .as_bytes(),
    )
}
pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
pub(crate) fn fts_query(query: &str) -> String {
    let terms = query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    terms.join(" OR ")
}
