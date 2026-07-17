use super::*;

/// Resolve the ONE binding target a bind request names, or `Ok(None)` when the request names none
/// — an UNANCHORED node (#463): a `Concept` or standalone `Task` lives only as a graph node, with
/// no code anchor. This is the single source of truth for "is a binding present"; each caller
/// decides what to do with `None` (`create_memory` allows it; `rebind_memory` rejects it — a rebind
/// with nothing to bind to is meaningless). `Err` is reserved for a NAMED-but-unresolvable target.
pub fn resolve_binding(
    conn: &Connection,
    bind: &RepoMemoryBindTarget,
) -> anyhow::Result<Option<ResolvedBinding>> {
    if let Some(logical_symbol_id) = bind.logical_symbol_id {
        return resolve_logical_symbol_binding(conn, logical_symbol_id).map(Some);
    }
    if let Some(symbol_id) = bind.symbol_id {
        return resolve_symbol_binding(conn, symbol_id).map(Some);
    }
    if let Some(chunk_id) = bind.chunk_id {
        return resolve_chunk_binding(conn, chunk_id).map(Some);
    }
    if let Some(edge_id) = bind.edge_id {
        return resolve_edge_binding(conn, edge_id).map(Some);
    }
    // Server-derived call path (preferred): compute the authoritative hash from the edges.
    if let Some(edge_path) = bind.edge_path.as_deref() {
        return resolve_call_path_from_edges(conn, bind, edge_path).map(Some);
    }
    if let Some(edge_sequence_hash) = bind.edge_sequence_hash.as_deref() {
        return resolve_call_path_binding(conn, bind, edge_sequence_hash).map(Some);
    }
    if let Some(dir) = bind.dir.as_deref() {
        return resolve_dir_binding(conn, dir).map(Some);
    }
    if let Some(path) = bind.path.as_deref() {
        return resolve_path_binding(conn, path, bind.start_line, bind.end_line).map(Some);
    }
    if let Some(commit_hash) = bind.commit_hash.as_deref() {
        return Ok(Some(ResolvedBinding {
            binding_kind: "commit".to_string(),
            binding_id: commit_hash.to_string(),
            path: None,
            start_line: None,
            end_line: None,
            logical_symbol_id: None,
            symbol_id: None,
            chunk_id: None,
            edge_id: None,
            commit_hash: Some(commit_hash.to_string()),
            tracker: None,
            project: None,
            item_key: None,
            symbol_kind: None,
            signature_hash: None,
            call_path: None,
            source_text_hash: None,
            anchor_status: "unverified".to_string(),
        }));
    }
    if let (Some(tracker), Some(project), Some(item_key)) =
        (bind.tracker.as_deref(), bind.project.as_deref(), bind.item_key.as_deref())
    {
        // The tracker token set is CLOSED — reject an unknown provider instead of persisting a
        // free-form string the papertrail readers would never resolve.
        let tracker = rag_rat_papertrail::Tracker::from_db_str(tracker)?;
        return Ok(Some(ResolvedBinding {
            binding_kind: "tracker".to_string(),
            binding_id: format!("{}:{project}#{item_key}", tracker.as_db_str()),
            path: None,
            start_line: None,
            end_line: None,
            logical_symbol_id: None,
            symbol_id: None,
            chunk_id: None,
            edge_id: None,
            commit_hash: None,
            tracker: Some(tracker.as_db_str().to_string()),
            project: Some(project.to_string()),
            item_key: Some(item_key.to_string()),
            symbol_kind: None,
            signature_hash: None,
            call_path: None,
            source_text_hash: None,
            anchor_status: "unverified".to_string(),
        }));
    }
    // Fell through every binding branch. A TRULY EMPTY target is an unanchored node (#463); a
    // PARTIALLY populated one (e.g. a tracker+project without an item_key, a span without a path,
    // or call-path metadata without an `edge_sequence_hash`/`edge_path`) is a malformed anchor —
    // reject it rather than silently dropping the intended binding into an invisible unanchored
    // memory.
    if bind.is_empty() {
        return Ok(None);
    }
    anyhow::bail!(
        "memory_create binding is incomplete: give a full logical_symbol_id, symbol_id, chunk_id, \
         edge_id, call path, path/span, commit_hash, or tracker (tracker+project+item_key) ref — \
         or omit `bind` entirely to create an unanchored node"
    )
}

/// Normalize a directory anchor: trim, drop a leading `./`, strip a trailing `/`.
/// The repo root is the empty string.
pub(crate) fn normalize_dir(dir: &str) -> String {
    dir.trim().trim_start_matches("./").trim_end_matches('/').to_string()
}

/// Resolve a `dir` bind target: `binding_kind="dir"`, `binding_id`=normalized dir,
/// `anchor_status`="current" iff at least one indexed file lives at or under the dir,
/// else "gone". The repo root (empty string) is current whenever any file is indexed.
pub(crate) fn resolve_dir_binding(conn: &Connection, dir: &str) -> anyhow::Result<ResolvedBinding> {
    let dir = normalize_dir(dir);
    let exists = dir_has_files(conn, &dir)?;
    Ok(ResolvedBinding {
        binding_kind: "dir".to_string(),
        binding_id: dir.clone(),
        path: Some(dir),
        start_line: None,
        end_line: None,
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        symbol_kind: None,
        signature_hash: None,
        call_path: None,
        source_text_hash: None,
        anchor_status: if exists { "current" } else { "gone" }.to_string(),
    })
}

/// Read: are there indexed files at or under `dir`?
/// Root (`""`) matches any indexed file.
pub(crate) fn dir_has_files(conn: &Connection, dir: &str) -> anyhow::Result<bool> {
    let n: i64 = if dir.is_empty() {
        conn.query_row("SELECT EXISTS(SELECT 1 FROM files)", [], |r| r.get(0))?
    } else {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1 OR path LIKE ?1 || '/%')",
            [dir],
            |r| r.get(0),
        )?
    };
    Ok(n != 0)
}

pub(crate) fn resolve_logical_symbol_binding(
    conn: &Connection,
    logical_symbol_id: i64,
) -> anyhow::Result<ResolvedBinding> {
    let logical = crate::symbol::lookup_logical_by_id(conn, logical_symbol_id)?
        .ok_or_else(|| anyhow::anyhow!("logical_symbol_id {logical_symbol_id} not found"))?;
    let chunk = chunk_for_logical_symbol(conn, logical_symbol_id)?;
    let member_symbol_id = chunk.as_ref().and_then(|c| c.symbol_id);
    let (kind, sig_hash) = match member_symbol_id {
        Some(sid) => symbol_signal(conn, sid)?,
        None => (None, None),
    };
    Ok(ResolvedBinding {
        binding_kind: "logical_symbol".to_string(),
        binding_id: logical.qualified_name,
        path: Some(logical.path),
        start_line: chunk.as_ref().map(|chunk| chunk.start_line),
        end_line: chunk.as_ref().map(|chunk| chunk.end_line),
        logical_symbol_id: Some(logical_symbol_id),
        symbol_id: member_symbol_id,
        chunk_id: chunk.as_ref().map(|chunk| chunk.chunk_id),
        edge_id: None,
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        symbol_kind: kind,
        signature_hash: sig_hash,
        call_path: None,
        source_text_hash: chunk.map(|chunk| chunk.text_hash),
        anchor_status: "current".to_string(),
    })
}
pub(crate) fn symbol_signal(
    conn: &Connection,
    symbol_id: i64,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let row = conn
        .query_row("SELECT kind, signature FROM symbols WHERE id = ?1", [symbol_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .optional()?;
    Ok(match row {
        Some((kind, signature)) =>
            (Some(kind), signature.map(|sig| hex_sha256(sig.trim().as_bytes()))),
        None => (None, None),
    })
}
pub(crate) fn resolve_symbol_binding(
    conn: &Connection,
    symbol_id: i64,
) -> anyhow::Result<ResolvedBinding> {
    let symbol = crate::symbol::lookup_by_id(conn, symbol_id)?
        .ok_or_else(|| anyhow::anyhow!("symbol_id {symbol_id} not found"))?;
    let chunk = chunk_for_symbol(conn, symbol_id, &symbol.qualified_name)?;
    let (kind, sig_hash) = symbol_signal(conn, symbol_id)?;
    Ok(ResolvedBinding {
        binding_kind: "symbol".to_string(),
        binding_id: symbol.qualified_name,
        path: Some(symbol.path),
        start_line: chunk.as_ref().map(|chunk| chunk.start_line),
        end_line: chunk.as_ref().map(|chunk| chunk.end_line),
        logical_symbol_id: symbol.logical_symbol_id,
        symbol_id: Some(symbol_id),
        chunk_id: chunk.as_ref().map(|chunk| chunk.chunk_id),
        edge_id: None,
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        symbol_kind: kind,
        signature_hash: sig_hash,
        call_path: None,
        source_text_hash: chunk.map(|chunk| chunk.text_hash),
        anchor_status: "current".to_string(),
    })
}
pub(crate) fn resolve_chunk_binding(
    conn: &Connection,
    chunk_id: i64,
) -> anyhow::Result<ResolvedBinding> {
    let chunk = chunk_by_id(conn, chunk_id)?
        .ok_or_else(|| anyhow::anyhow!("chunk_id {chunk_id} not found"))?;
    let symbol_id = symbol_id_for_chunk(conn, &chunk)?;
    Ok(ResolvedBinding {
        binding_kind: "chunk".to_string(),
        binding_id: chunk_id.to_string(),
        path: Some(chunk.path),
        start_line: Some(chunk.start_line),
        end_line: Some(chunk.end_line),
        logical_symbol_id: symbol_id
            .and_then(|id| logical_symbol_id_for_symbol(conn, id).ok().flatten()),
        symbol_id,
        chunk_id: Some(chunk_id),
        edge_id: None,
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        symbol_kind: None,
        signature_hash: None,
        call_path: None,
        source_text_hash: Some(chunk.text_hash),
        anchor_status: "current".to_string(),
    })
}
pub(crate) fn resolve_edge_binding(
    conn: &Connection,
    edge_id: i64,
) -> anyhow::Result<ResolvedBinding> {
    let edge =
        edge_by_id(conn, edge_id)?.ok_or_else(|| anyhow::anyhow!("edge_id {edge_id} not found"))?;
    Ok(ResolvedBinding {
        binding_kind: "edge".to_string(),
        binding_id: edge.fingerprint,
        path: Some(edge.path),
        start_line: Some(edge.start_line),
        end_line: Some(edge.end_line),
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: Some(edge_id),
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        symbol_kind: None,
        signature_hash: None,
        call_path: None,
        source_text_hash: Some(edge.source_hash),
        anchor_status: "current".to_string(),
    })
}
pub(crate) fn resolve_call_path_binding(
    conn: &Connection,
    bind: &RepoMemoryBindTarget,
    edge_sequence_hash: &str,
) -> anyhow::Result<ResolvedBinding> {
    validate_len("edge_sequence_hash", edge_sequence_hash, 128)?;
    let path_summary = bind
        .path_summary
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("call-path memory requires path_summary"))?;
    validate_len("path_summary", path_summary, 500)?;
    if let Some(start_id) = bind.start_logical_symbol_id {
        ensure_logical_symbol_exists(conn, start_id)?;
    }
    if let Some(end_id) = bind.end_logical_symbol_id {
        ensure_logical_symbol_exists(conn, end_id)?;
    }
    Ok(ResolvedBinding {
        binding_kind: "call_path".to_string(),
        binding_id: edge_sequence_hash.to_string(),
        path: None,
        start_line: None,
        end_line: None,
        logical_symbol_id: bind.start_logical_symbol_id.or(bind.end_logical_symbol_id),
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        symbol_kind: None,
        signature_hash: None,
        call_path: Some(ResolvedCallPath {
            start_logical_symbol_id: bind.start_logical_symbol_id,
            end_logical_symbol_id: bind.end_logical_symbol_id,
            edge_sequence_hash: edge_sequence_hash.to_string(),
            path_summary: path_summary.to_string(),
            // Client-supplied hash: no server-resolved edges to persist, so it stays unverified.
            edges: Vec::new(),
        }),
        source_text_hash: None,
        anchor_status: "unverified".to_string(),
    })
}
pub(crate) fn resolve_path_binding(
    conn: &Connection,
    path: &str,
    start_line: Option<i64>,
    end_line: Option<i64>,
) -> anyhow::Result<ResolvedBinding> {
    let file_hash = conn
        .query_row(
            "SELECT sha256 FROM files WHERE path = ?1 ORDER BY id DESC LIMIT 1",
            [path],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(ResolvedBinding {
        binding_kind: "path".to_string(),
        binding_id: match (start_line, end_line) {
            (Some(start), Some(end)) => format!("{path}:{start}-{end}"),
            _ => path.to_string(),
        },
        path: Some(path.to_string()),
        start_line,
        end_line,
        logical_symbol_id: None,
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        symbol_kind: None,
        signature_hash: None,
        call_path: None,
        source_text_hash: file_hash,
        anchor_status: "current".to_string(),
    })
}
pub(crate) fn chunk_by_id(conn: &Connection, chunk_id: i64) -> anyhow::Result<Option<ChunkAnchor>> {
    conn.query_row(
        "
        SELECT chunks.id AS chunk_id,
               files.path AS path,
               chunks.start_line AS start_line,
               chunks.end_line AS end_line,
               chunks.symbol_path AS symbol_path,
               chunks.text_hash AS text_hash,
               NULL AS symbol_id
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        WHERE chunks.id = ?1
        ",
        [chunk_id],
        chunk_anchor_row,
    )
    .optional()
    .map_err(Into::into)
}
pub(crate) fn chunk_for_symbol(
    conn: &Connection,
    symbol_id: i64,
    qualified_name: &str,
) -> anyhow::Result<Option<ChunkAnchor>> {
    conn.query_row(
        "
        SELECT chunks.id AS chunk_id,
               files.path AS path,
               chunks.start_line AS start_line,
               chunks.end_line AS end_line,
               chunks.symbol_path AS symbol_path,
               chunks.text_hash AS text_hash,
               symbols.id AS symbol_id
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        LEFT JOIN chunks ON chunks.file_id = files.id
            AND (chunks.symbol_path = qn.value OR chunks.symbol_path = ?2)
        WHERE symbols.id = ?1
        ORDER BY CASE WHEN chunks.symbol_path = qn.value THEN 0 ELSE 1 END,
                 chunks.start_line
        LIMIT 1
        ",
        params![symbol_id, qualified_name],
        chunk_anchor_row,
    )
    .optional()
    .map_err(Into::into)
}
pub(crate) fn chunk_for_logical_symbol(
    conn: &Connection,
    logical_symbol_id: i64,
) -> anyhow::Result<Option<ChunkAnchor>> {
    conn.query_row(
        "
        SELECT chunks.id AS chunk_id,
               files.path AS path,
               chunks.start_line AS start_line,
               chunks.end_line AS end_line,
               chunks.symbol_path AS symbol_path,
               chunks.text_hash AS text_hash,
               symbols.id AS symbol_id
        FROM logical_symbol_members
        JOIN symbols ON symbols.id = logical_symbol_members.symbol_id
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        LEFT JOIN chunks ON chunks.file_id = files.id
            AND chunks.symbol_path = qn.value
        WHERE logical_symbol_members.logical_symbol_id = ?1
        ORDER BY logical_symbol_members.start_line, chunks.start_line
        LIMIT 1
        ",
        [logical_symbol_id],
        chunk_anchor_row,
    )
    .optional()
    .map_err(Into::into)
}
pub(crate) fn chunk_ids_for_symbol(
    conn: &Connection,
    symbol: &crate::symbol::SymbolHit,
) -> anyhow::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "
        SELECT chunks.id AS chunk_id
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        WHERE files.path = ?1
          AND (chunks.symbol_path = ?2 OR chunks.symbol_path = ?3)
        ",
    )?;
    let rows = stmt
        .query_map(params![symbol.path, symbol.qualified_name, symbol.symbol_path], |row| {
            row.get::<_, i64>("chunk_id")
        })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}
pub(crate) fn symbol_id_for_chunk(
    conn: &Connection,
    chunk: &ChunkAnchor,
) -> anyhow::Result<Option<i64>> {
    let Some(symbol_path) = chunk.symbol_path.as_deref() else {
        return Ok(None);
    };
    conn.query_row(
        "
        SELECT symbols.id AS symbol_id
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE files.path = ?1
          AND symbols.qualified_name_id = (SELECT id FROM name_strings WHERE value = ?2)
        LIMIT 1
        ",
        params![chunk.path, symbol_path],
        |row| row.get("symbol_id"),
    )
    .optional()
    .map_err(Into::into)
}
pub(crate) fn logical_symbol_id_for_symbol(
    conn: &Connection,
    symbol_id: i64,
) -> anyhow::Result<Option<i64>> {
    conn.query_row(
        "SELECT logical_symbol_id AS logical_symbol_id FROM logical_symbol_members WHERE \
         symbol_id = ?1 LIMIT 1",
        [symbol_id],
        |row| row.get("logical_symbol_id"),
    )
    .optional()
    .map_err(Into::into)
}
pub(crate) fn chunk_anchor_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkAnchor> {
    Ok(ChunkAnchor {
        chunk_id: row.get("chunk_id")?,
        path: row.get("path")?,
        start_line: row.get("start_line")?,
        end_line: row.get("end_line")?,
        symbol_path: row.get("symbol_path")?,
        text_hash: row.get("text_hash")?,
        symbol_id: row.get("symbol_id")?,
    })
}
pub(crate) fn edge_by_id(conn: &Connection, edge_id: i64) -> anyhow::Result<Option<EdgeAnchor>> {
    conn.query_row(
        "
        SELECT edges.id AS edge_id,
               files.path AS path,
               COALESCE(NULLIF(edges.source_start_line, 0), 1) AS start_line,
               COALESCE(NULLIF(edges.source_end_line, 0), NULLIF(edges.source_start_line, 0), 1) \
         AS end_line,
               files.sha256 AS source_hash,
               edges.from_name AS from_name,
               edges.to_name AS to_name,
               edges.edge_kind AS edge_kind,
               edges.target_qualified_name AS target_qualified_name,
               edges.receiver_hint AS receiver_hint
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edges.id = ?1
        ",
        [edge_id],
        edge_anchor_row,
    )
    .optional()
    .map_err(Into::into)
}
pub(crate) fn edge_by_fingerprint(
    conn: &Connection,
    fingerprint: &str,
) -> anyhow::Result<Option<EdgeAnchor>> {
    let mut stmt = conn.prepare(
        "
        SELECT edges.id AS edge_id,
               files.path AS path,
               COALESCE(NULLIF(edges.source_start_line, 0), 1) AS start_line,
               COALESCE(NULLIF(edges.source_end_line, 0), NULLIF(edges.source_start_line, 0), 1) \
         AS end_line,
               files.sha256 AS source_hash,
               edges.from_name AS from_name,
               edges.to_name AS to_name,
               edges.edge_kind AS edge_kind,
               edges.target_qualified_name AS target_qualified_name,
               edges.receiver_hint AS receiver_hint
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        ",
    )?;
    let rows = stmt.query_map([], edge_anchor_row)?;
    for row in rows {
        let edge = row?;
        if edge.fingerprint == fingerprint {
            return Ok(Some(edge));
        }
    }
    Ok(None)
}
pub(crate) fn edge_anchor_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EdgeAnchor> {
    let path: String = row.get("path")?;
    let start_line = row.get("start_line")?;
    let end_line = row.get("end_line")?;
    let from_name: Option<String> = row.get("from_name")?;
    let to_name: Option<String> = row.get("to_name")?;
    let edge_kind: String = row.get("edge_kind")?;
    let target_qualified_name: Option<String> = row.get("target_qualified_name")?;
    let receiver_hint: Option<String> = row.get("receiver_hint")?;
    Ok(EdgeAnchor {
        edge_id: row.get("edge_id")?,
        fingerprint: edge_fingerprint(EdgeFingerprintParts {
            path: &path,
            start_line,
            end_line,
            from_name: from_name.as_deref(),
            to_name: to_name.as_deref(),
            edge_kind: &edge_kind,
            target_qualified_name: target_qualified_name.as_deref(),
            receiver_hint: receiver_hint.as_deref(),
        }),
        path,
        start_line,
        end_line,
        source_hash: row.get("source_hash")?,
    })
}
pub(crate) fn edge_fingerprint(parts: EdgeFingerprintParts<'_>) -> String {
    hex_sha256(
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            parts.path,
            parts.start_line,
            parts.end_line,
            parts.from_name.unwrap_or(""),
            parts.to_name.unwrap_or(""),
            parts.edge_kind,
            parts.target_qualified_name.unwrap_or(""),
            parts.receiver_hint.unwrap_or("")
        )
        .as_bytes(),
    )
}
/// Hash-algorithm version prefix for server-derived call-path hashes. Bump if the input
/// composition changes so old hashes never silently collide with a new scheme (#38).
pub(crate) const CALL_PATH_HASH_VERSION: &str = "cp1";
/// Cap on edges in one call path — bounds the bind payload and the validation scan.
const MAX_CALL_PATH_EDGES: usize = 64;

/// Authoritative edge-sequence hash: versioned SHA-256 over the ordered edge fingerprints.
/// Built from `edge_fingerprint` (row-id-independent), so it survives reindexing and edge row
/// churn as long as the call sites' content is unchanged.
pub(crate) fn compute_edge_sequence_hash<'a>(
    fingerprints: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut buf = String::from(CALL_PATH_HASH_VERSION);
    for fingerprint in fingerprints {
        buf.push('\n');
        buf.push_str(fingerprint);
    }
    hex_sha256(buf.as_bytes())
}

/// Read one edge's fingerprint + loose identity by row id. The fingerprint is computed exactly
/// as `edge_anchor_row` does (same columns, same Option handling) so it matches
/// `edge_by_fingerprint` during validation.
pub(crate) fn call_path_edge_by_id(
    conn: &Connection,
    edge_id: i64,
) -> anyhow::Result<Option<CallPathEdge>> {
    conn.query_row(
        "
        SELECT files.path AS path,
               COALESCE(NULLIF(edges.source_start_line, 0), 1) AS start_line,
               COALESCE(NULLIF(edges.source_end_line, 0), NULLIF(edges.source_start_line, 0), 1) \
         AS end_line,
               edges.from_name AS from_name,
               edges.to_name AS to_name,
               edges.edge_kind AS edge_kind,
               edges.target_qualified_name AS target_qualified_name,
               edges.receiver_hint AS receiver_hint
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edges.id = ?1
        ",
        [edge_id],
        |row| {
            let path: String = row.get("path")?;
            let start_line: i64 = row.get("start_line")?;
            let end_line: i64 = row.get("end_line")?;
            let from_name: Option<String> = row.get("from_name")?;
            let to_name: Option<String> = row.get("to_name")?;
            let edge_kind: String = row.get("edge_kind")?;
            let target_qualified_name: Option<String> = row.get("target_qualified_name")?;
            let receiver_hint: Option<String> = row.get("receiver_hint")?;
            let fingerprint = edge_fingerprint(EdgeFingerprintParts {
                path: &path,
                start_line,
                end_line,
                from_name: from_name.as_deref(),
                to_name: to_name.as_deref(),
                edge_kind: &edge_kind,
                target_qualified_name: target_qualified_name.as_deref(),
                receiver_hint: receiver_hint.as_deref(),
            });
            Ok(CallPathEdge {
                fingerprint,
                from_name,
                to_name,
                edge_kind,
                target_qualified_name,
                receiver_hint,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Resolve a server-derived call-path binding from ordered edge ids: look each edge up, compute
/// the authoritative `edge_sequence_hash` from their fingerprints, and carry the ordered edges so
/// `insert_binding` can persist them for validation. `anchor_status` is `current` (the edges all
/// resolve right now). A client-supplied `edge_sequence_hash`, if any, is ignored in favor of the
/// server-derived one.
pub(crate) fn resolve_call_path_from_edges(
    conn: &Connection,
    bind: &RepoMemoryBindTarget,
    edge_ids: &[i64],
) -> anyhow::Result<ResolvedBinding> {
    if edge_ids.is_empty() {
        anyhow::bail!("call-path binding requires at least one edge id in edge_path");
    }
    if edge_ids.len() > MAX_CALL_PATH_EDGES {
        anyhow::bail!("call path has {} edges; the limit is {MAX_CALL_PATH_EDGES}", edge_ids.len());
    }
    let mut edges = Vec::with_capacity(edge_ids.len());
    for &edge_id in edge_ids {
        let edge = call_path_edge_by_id(conn, edge_id)?.ok_or_else(|| {
            anyhow::anyhow!("edge_path references edge {edge_id}, which is not in the index")
        })?;
        edges.push(edge);
    }
    let hash = compute_edge_sequence_hash(edges.iter().map(|edge| edge.fingerprint.as_str()));

    if let Some(start_id) = bind.start_logical_symbol_id {
        ensure_logical_symbol_exists(conn, start_id)?;
    }
    if let Some(end_id) = bind.end_logical_symbol_id {
        ensure_logical_symbol_exists(conn, end_id)?;
    }

    // Prefer the caller's summary; otherwise synthesize a readable "a -> b -> c" from the edge
    // target names so the stored row is never empty.
    let path_summary = match bind.path_summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(summary) => summary.to_string(),
        None => default_path_summary(&edges),
    };
    validate_len("path_summary", &path_summary, 500)?;

    Ok(ResolvedBinding {
        binding_kind: "call_path".to_string(),
        binding_id: hash.clone(),
        path: None,
        start_line: None,
        end_line: None,
        logical_symbol_id: bind.start_logical_symbol_id.or(bind.end_logical_symbol_id),
        symbol_id: None,
        chunk_id: None,
        edge_id: None,
        commit_hash: None,
        tracker: None,
        project: None,
        item_key: None,
        symbol_kind: None,
        signature_hash: None,
        call_path: Some(ResolvedCallPath {
            start_logical_symbol_id: bind.start_logical_symbol_id,
            end_logical_symbol_id: bind.end_logical_symbol_id,
            edge_sequence_hash: hash,
            path_summary,
            edges,
        }),
        source_text_hash: None,
        anchor_status: "current".to_string(),
    })
}

/// `"caller -> a -> b"` from the ordered edges, capped to fit `path_summary`'s 500-char limit.
fn default_path_summary(edges: &[CallPathEdge]) -> String {
    let mut parts = Vec::with_capacity(edges.len() + 1);
    if let Some(from) = edges.first().and_then(|edge| edge.from_name.as_deref()) {
        parts.push(from.to_string());
    }
    for edge in edges {
        parts.push(edge.to_name.clone().unwrap_or_else(|| "?".to_string()));
    }
    let summary = parts.join(" -> ");
    summary.chars().take(500).collect()
}

pub(crate) fn ensure_logical_symbol_exists(
    conn: &Connection,
    logical_symbol_id: i64,
) -> anyhow::Result<()> {
    if crate::symbol::lookup_logical_by_id(conn, logical_symbol_id)?.is_some() {
        return Ok(());
    }
    anyhow::bail!("logical_symbol_id {logical_symbol_id} not found")
}
pub fn insert_binding(
    conn: &Connection,
    memory_id: &str,
    binding: &ResolvedBinding,
    now: i64,
) -> anyhow::Result<()> {
    conn.execute(
        "
        INSERT INTO repo_memory_bindings(
            memory_id, binding_kind, binding_id, path, start_line, end_line, logical_symbol_id,
            symbol_id, chunk_id, edge_id, commit_hash, tracker, project, item_key,
            symbol_kind, signature_hash, anchor_status, created_at_ms
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
        ",
        params![
            memory_id,
            binding.binding_kind,
            binding.binding_id,
            binding.path,
            binding.start_line,
            binding.end_line,
            binding.logical_symbol_id,
            binding.symbol_id,
            binding.chunk_id,
            binding.edge_id,
            binding.commit_hash,
            binding.tracker,
            binding.project,
            binding.item_key,
            binding.symbol_kind,
            binding.signature_hash,
            binding.anchor_status,
            now
        ],
    )?;
    if let Some(call_path) = &binding.call_path {
        conn.execute(
            "
            INSERT INTO repo_memory_call_paths(
                memory_id, start_logical_symbol_id, end_logical_symbol_id, edge_sequence_hash,
                path_summary, created_at_ms
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
            params![
                memory_id,
                call_path.start_logical_symbol_id,
                call_path.end_logical_symbol_id,
                call_path.edge_sequence_hash,
                call_path.path_summary,
                now
            ],
        )?;
        // Persist the ordered edges behind a server-derived hash so validation can re-check them
        // (#38). A legacy client-supplied hash carries no edges and stays unverified.
        for (ordinal, edge) in call_path.edges.iter().enumerate() {
            conn.execute(
                "
                INSERT INTO repo_memory_call_path_edges(
                    memory_id, edge_sequence_hash, ordinal, edge_fingerprint, from_name, to_name,
                    edge_kind, target_qualified_name, receiver_hint
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ",
                params![
                    memory_id,
                    call_path.edge_sequence_hash,
                    ordinal as i64,
                    edge.fingerprint,
                    edge.from_name,
                    edge.to_name,
                    edge.edge_kind,
                    edge.target_qualified_name,
                    edge.receiver_hint,
                ],
            )?;
        }
    }
    Ok(())
}
/// Stamp every `repo_memory_bindings` row of `memory_id` with the PARENT memory's `repo_id` (spec
/// §4.5: a binding inherits its memory's repo). Sourced from `repo_memories.repo_id`, NOT the
/// active repo — a memory bound under a repo other than the currently-active one (a future
/// cross-repo shape) keeps its own attribution. Called after every binding (re-)insert
/// (`create_memory` after its memory is stamped, `rebind_memory` which deletes + re-inserts), so a
/// re-inserted binding never strands at the `__unassigned__` default — which would drop it out of
/// the binding-scoped sweeps (`validate_memories`, `doctor_report`, `anchor_health_counts`) while
/// the parent memory sat in the active repo. No-op pre-A5 (no `repo_id` column).
/// `repo_memory_call_paths` / `repo_memory_call_path_edges` are transitive (scoped via the
/// `repo_memories` FK), so they carry no `repo_id` to stamp.
pub fn stamp_bindings_from_parent_repo(conn: &Connection, memory_id: &str) -> anyhow::Result<()> {
    if rag_rat_db::schema::periphery_repo_scope(conn, "repo_memory_bindings")?.is_some() {
        conn.execute(
            "UPDATE repo_memory_bindings
             SET repo_id = (SELECT repo_id FROM repo_memories WHERE id = ?1)
             WHERE memory_id = ?1",
            [memory_id],
        )?;
    }
    Ok(())
}
pub(crate) struct RelocateMatch {
    pub(crate) binding_id: String,
    pub(crate) symbol_id: i64,
    pub(crate) logical_symbol_id: Option<i64>,
    pub(crate) path: String,
    pub(crate) chunk_id: Option<i64>,
    pub(crate) start_line: Option<i64>,
    pub(crate) end_line: Option<i64>,
    pub(crate) symbol_kind: Option<String>,
    pub(crate) signature_hash: Option<String>,
}

/// Find the unique moved home of a symbol whose stored anchor is gone.
/// `short_name` = the symbol name with its old `"{path}::"` prefix stripped.
/// Relocation requires a content-hash match (`chunk.text_hash == source_text_hash`);
/// kind/signature corroborate, never override. Returns `Some` only when exactly one
/// candidate content-matches; two or more → `None` (ambiguous → stay gone).
pub(crate) fn relocate_symbol_by_name(
    conn: &Connection,
    short_name: &str,
    source_text_hash: &str,
) -> anyhow::Result<Option<RelocateMatch>> {
    let mut stmt = conn.prepare(
        // Content-hash confirmation is mandatory for a silent cross-file relocate.
        // The join on `files` keeps this context-scoped, matching the qualified_name fallback
        // above.
        "
        SELECT symbols.id AS symbol_id, qn.value AS qualified_name,
               files.path AS path, symbols.kind AS kind, symbols.signature AS signature
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        WHERE symbols.name = ?1
        ",
    )?;
    let rows = stmt.query_map([short_name], |row| {
        Ok((
            row.get::<_, i64>("symbol_id")?,
            row.get::<_, String>("qualified_name")?,
            row.get::<_, String>("path")?,
            row.get::<_, String>("kind")?,
            row.get::<_, Option<String>>("signature")?,
        ))
    })?;
    let mut matched: Option<RelocateMatch> = None;
    for row in rows {
        let (symbol_id, qualified_name, path, kind, signature) = row?;
        let chunk = chunk_for_symbol(conn, symbol_id, &qualified_name)?;
        let text_hash = chunk.as_ref().map(|c| c.text_hash.as_str());
        if text_hash != Some(source_text_hash) {
            continue; // content-hash is required for a silent relocate
        }
        if matched.is_some() {
            return Ok(None); // >=2 content matches -> ambiguous -> stay gone
        }
        matched = Some(RelocateMatch {
            binding_id: qualified_name,
            symbol_id,
            logical_symbol_id: logical_symbol_id_for_symbol(conn, symbol_id)?,
            path,
            chunk_id: chunk.as_ref().map(|c| c.chunk_id),
            start_line: chunk.as_ref().map(|c| c.start_line),
            end_line: chunk.as_ref().map(|c| c.end_line),
            symbol_kind: Some(kind),
            signature_hash: signature.map(|s| hex_sha256(s.trim().as_bytes())),
        });
    }
    Ok(matched)
}

/// Find the unique live chunk whose text_hash matches `source_text_hash`.
/// Content-hash match is mandatory; two or more matches (>=2) → `None` (ambiguous → stay gone).
pub(crate) fn relocate_chunk_by_hash(
    conn: &Connection,
    source_text_hash: &str,
) -> anyhow::Result<Option<ChunkAnchor>> {
    let mut stmt = conn.prepare(
        "
        SELECT chunks.id AS chunk_id, files.path AS path, chunks.start_line AS start_line,
               chunks.end_line AS end_line, chunks.symbol_path AS symbol_path,
               chunks.text_hash AS text_hash, NULL AS symbol_id
        FROM chunks JOIN files ON files.id = chunks.file_id
        WHERE chunks.text_hash = ?1
        ",
    )?;
    let mut rows = stmt.query_map([source_text_hash], chunk_anchor_row)?;
    let Some(first) = rows.next() else { return Ok(None) };
    let first = first?;
    if rows.next().is_some() {
        return Ok(None); // >=2 -> ambiguous -> stay gone
    }
    Ok(Some(first))
}

/// Strip the persisted `"{path}::"` prefix from a path-qualified `binding_id`.
/// Falls back to last-`::` split only when `path` is absent or not a prefix of `binding_id`.
pub(crate) fn short_symbol_name<'a>(binding_id: &'a str, path: Option<&str>) -> &'a str {
    if let Some(path) = path
        && let Some(rest) = binding_id.strip_prefix(path)
        && let Some(name) = rest.strip_prefix("::")
    {
        return name;
    }
    binding_id.rsplit("::").next().unwrap_or(binding_id)
}
