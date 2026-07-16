use rusqlite::{Transaction, TransactionBehavior};

use super::*;
use crate::query::memory::authoring;

/// Max chars for a memory title (a one-line summary) and body. The body cap is generous on purpose:
/// Invariant / Decision / BugPattern memories are meant to carry the *why* + *how to apply* in
/// detail, and 4 000 forced real content out (the MCP `memory_create`/`memory_update` schemas
/// document these). Enforced in Rust, not the schema, so raising them needs no migration.
pub(crate) const MAX_MEMORY_TITLE_LEN: usize = 160;
pub(crate) const MAX_MEMORY_BODY_LEN: usize = 8000;

pub(crate) fn create_memory(
    conn: &Connection,
    request: RepoMemoryCreate,
) -> anyhow::Result<RepoMemoryCreateResult> {
    validate_kind(&request.kind)?;
    validate_confidence(&request.confidence)?;
    validate_len("title", &request.title, MAX_MEMORY_TITLE_LEN)?;
    validate_len("body", &request.body, MAX_MEMORY_BODY_LEN)?;
    let source = request.source.clone().unwrap_or_else(|| "agent".to_string());
    validate_source(&source)?;
    validate_payload(&request.kind, request.payload_json.as_deref())?;
    // `None` = an UNANCHORED node: only the polymorphic graph-node kinds (`Task` / `Concept`) may
    // be anchorless (#463/#465). Every OTHER kind must anchor to code — a zero-binding one is
    // an orphan the dream verifier flags, so allowing its create would generate self-inflicted
    // `memory_unverifiable` noise. This gate stays in lock-step with `dream::unverifiable_findings`
    // via the shared `is_polymorphic_node_kind`.
    let binding = resolve_binding(conn, &request.bind)?;
    if binding.is_none() && !is_polymorphic_node_kind(&request.kind) {
        anyhow::bail!(
            "a `{}` memory must anchor to code (only Task/Concept may be unanchored)",
            request.kind
        );
    }
    let input_hash = memory_input_hash(
        &request.kind,
        &request.title,
        &request.body,
        &request.tags,
        request.payload_json.as_deref(),
    );
    if let Some(existing_id) = duplicate_memory_id(
        conn,
        &request.kind,
        &request.title,
        &request.body,
        request.payload_json.as_deref(),
        binding.as_ref(),
    )? {
        let memory = memory_by_id(conn, &existing_id)?
            .ok_or_else(|| anyhow::anyhow!("duplicate memory `{existing_id}` disappeared"))?;
        return Ok(RepoMemoryCreateResult { memory, duplicate: true });
    }

    let now = now_ms();
    // The active-repo scope is folded into the id derivation (post-A5): without it, two repos
    // creating IDENTICAL content in the same millisecond derive the same id — the repo-scoped
    // dedupe above correctly passes, and the INSERT explodes on the global PK. The same resolved
    // scope drives the repo stamp below.
    let scope = memory_repo_scope(conn)?;
    let id = memory_id(now, &input_hash, &scope);
    // Backfill the pre-existing history BEFORE this live entry (idempotent; a cheap no-op once the
    // chain exists), then do the table writes + the op-append in ONE transaction so they commit —
    // or roll back — together (strict-atomic). Writes via `conn` participate in the open txn.
    authoring::backfill_memory_oplog(conn, now)?;
    // Authored write: commit durably so a `memory_create` that returned success survives power loss
    // (#560). FULL for this transaction only; the connection restores NORMAL on drop.
    let _durability = authoring::AuthoredDurability::begin(conn)?;
    let tx = conn.unchecked_transaction()?;
    conn.execute(
        "
        INSERT INTO repo_memories(
            id, kind, title, body, confidence, status, created_by, created_at_ms, updated_at_ms,
            source, payload_json, source_text_hash, input_hash, memory_version
        )
        VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?7, ?7, ?8, ?9, ?10, ?11, 'v1')
        ",
        params![
            id,
            request.kind,
            request.title,
            request.body,
            request.confidence,
            request.created_by,
            now,
            source,
            request.payload_json,
            binding.as_ref().and_then(|b| b.source_text_hash.clone()),
            input_hash
        ],
    )?;
    // Unanchored nodes (#463) skip binding writes entirely — nothing to insert or auto-moniker.
    if let Some(binding) = &binding {
        insert_binding(conn, &id, binding, now)?;
        // A symbol-bound memory whose logical symbol has a known SCIP moniker gets the moniker
        // anchor automatically (#70) — the relocation fallback the hash anchors can't provide.
        insert_auto_moniker_binding(conn, &id, binding, now)?;
    }
    // Stamp the active repo on the new memory, then stamp its bindings FROM the (now-stamped)
    // parent memory (spec §4.5: a binding defaults to its memory's repo). Gated on the
    // periphery scope — a no-op on the pre-A5 schema (no `repo_id` column). MUST run before
    // `upsert_memory_fts`, which mirrors `repo_memories.repo_id` into the FTS row.
    // `repo_memory_tags` / `repo_memory_call_paths` are transitive (scoped via the
    // `repo_memories` FK), so they are not stamped here.
    if let Some(repo_id) = &scope {
        conn.execute("UPDATE repo_memories SET repo_id = ?1 WHERE id = ?2", params![repo_id, id])?;
    }
    stamp_bindings_from_parent_repo(conn, &id)?;
    replace_tags(conn, &id, &request.tags)?;
    upsert_memory_fts(conn, &id)?;
    let memory = memory_by_id(conn, &id)?
        .ok_or_else(|| anyhow::anyhow!("created memory `{id}` could not be read back"))?;
    // Author the NodeCreate in the SAME txn; an authoring error drops `tx` → the INSERT rolls back.
    authoring::author_create(&tx, &memory, now)?;
    tx.commit()?;
    Ok(RepoMemoryCreateResult { memory, duplicate: false })
}
pub(crate) fn update_memory(
    conn: &Connection,
    update: RepoMemoryUpdate,
) -> anyhow::Result<RepoMemory> {
    let now = now_ms();
    // Backfill (idempotent) before opening our txn, then open an IMMEDIATE txn so the current-row
    // READ and the UPDATE are ONE atomic unit — a racing writer cannot flip the status between the
    // read and the write and desync the table from the op-log projection.
    authoring::backfill_memory_oplog(conn, now)?;
    // Authored write (content / status / obsolete): commit durably (#560), NORMAL restored on drop.
    let _durability = authoring::AuthoredDurability::begin(conn)?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let current = memory_by_id(conn, &update.memory_id)?
        .ok_or_else(|| anyhow::anyhow!("memory `{}` not found", update.memory_id))?;
    if let Some(kind) = update.kind.as_deref() {
        validate_kind(kind)?;
    }
    if let Some(confidence) = update.confidence.as_deref() {
        validate_confidence(confidence)?;
    }
    if let Some(status) = update.status.as_deref() {
        validate_status(status)?;
    }
    if let Some(title) = update.title.as_deref() {
        validate_len("title", title, MAX_MEMORY_TITLE_LEN)?;
    }
    if let Some(body) = update.body.as_deref() {
        validate_len("body", body, MAX_MEMORY_BODY_LEN)?;
    }
    // The resulting kind drives payload validation, the unanchored gate, and payload-clearing.
    let new_kind = update.kind.clone().unwrap_or_else(|| current.kind.clone());
    validate_payload(&new_kind, update.payload_json.as_deref())?;
    // The unanchored-kind invariant holds on UPDATE too — but only to PREVENT INTRODUCING it, not
    // to trap rows already in that state (a legacy pre-#465 unanchored `Decision`, or the
    // same-kind no-op of a status-only update / `mark_obsolete`, stays editable so it can be
    // CLEANED UP). Fire ONLY on a kind CHANGE to a non-polymorphic kind on a zero-binding node.
    let changing_kind = update.kind.as_deref().is_some_and(|kind| kind != current.kind);
    if changing_kind && !is_polymorphic_node_kind(&new_kind) && current.bindings.is_empty() {
        anyhow::bail!(
            "cannot retype an unanchored memory to `{new_kind}` (only Task/Concept may be \
             unanchored); bind it to code first"
        );
    }
    // A non-polymorphic kind carries NO payload — retyping AWAY from Task/Concept CLEARS a stranded
    // payload rather than preserving it. Otherwise keep the update's payload, else the current one.
    let stored_payload = if is_polymorphic_node_kind(&new_kind) {
        update.payload_json.clone().or_else(|| current.payload_json.clone())
    } else {
        None
    };
    // Detect what actually CHANGED before `current`'s fields are moved into the UPDATE params.
    // Content and status are INDEPENDENT LWW registers: author a NodeUpdate ONLY on a real content
    // change (else a status-only update would re-assert this device's content and could revert a
    // concurrent edit under sync) and a NodeStatus ONLY on a real status change.
    let new_title = update.title.as_deref().unwrap_or(current.title.as_str());
    let new_body = update.body.as_deref().unwrap_or(current.body.as_str());
    let new_confidence = update.confidence.as_deref().unwrap_or(current.confidence.as_str());
    // Compare tags NORMALIZED — `current.tags` is trimmed / non-empty / deduped / sorted (how
    // `replace_tags` stores and `tags_for_memory` reads them), so raw `update.tags` must be put in
    // the same shape before comparing, or a whitespace/duplicate-only re-tag would look "changed"
    // and mint a spurious NodeUpdate (the status-only-update hazard, re-reached through tags).
    let tags_changed = update.tags.as_ref().is_some_and(|raw| normalize_tags(raw) != current.tags);
    let content_changed = new_kind != current.kind
        || new_title != current.title
        || new_body != current.body
        || new_confidence != current.confidence
        || stored_payload != current.payload_json
        || tags_changed;
    let status_changed = update.status.as_deref().is_some_and(|s| s != current.status.as_str());
    conn.execute(
        "
        UPDATE repo_memories
        SET kind = ?2,
            title = ?3,
            body = ?4,
            confidence = ?5,
            status = ?6,
            payload_json = ?8,
            updated_at_ms = ?7
        WHERE id = ?1
        ",
        params![
            update.memory_id,
            new_kind,
            update.title.unwrap_or(current.title),
            update.body.unwrap_or(current.body),
            update.confidence.unwrap_or(current.confidence),
            update.status.unwrap_or(current.status),
            now,
            stored_payload
        ],
    )?;
    if let Some(tags) = update.tags {
        replace_tags(conn, &update.memory_id, &tags)?;
    }
    upsert_memory_fts(conn, &update.memory_id)?;
    let memory = memory_by_id(conn, &update.memory_id)?.ok_or_else(|| {
        anyhow::anyhow!("updated memory `{}` could not be read back", update.memory_id)
    })?;
    // Author NodeUpdate (+ NodeStatus on a status change) in the SAME txn; an authoring error drops
    // `tx` → the UPDATE rolls back.
    authoring::author_update(&tx, &memory, content_changed, status_changed, now)?;
    tx.commit()?;
    Ok(memory)
}
pub(crate) fn mark_obsolete(conn: &Connection, memory_id: &str) -> anyhow::Result<RepoMemory> {
    update_memory(conn, RepoMemoryUpdate {
        memory_id: memory_id.to_string(),
        kind: None,
        title: None,
        body: None,
        confidence: None,
        status: Some("obsolete".to_string()),
        tags: None,
        payload_json: None,
    })
}
pub(crate) fn memory_by_id(
    conn: &Connection,
    memory_id: &str,
) -> anyhow::Result<Option<RepoMemory>> {
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
pub(crate) fn memories_for_chunk(
    conn: &Connection,
    chunk_id: i64,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = memory_repo_scope_clause(&scope);
    let mut stmt = conn.prepare(&format!(
        "
        SELECT DISTINCT repo_memories.id AS memory_id
        FROM repo_memories
        JOIN repo_memory_bindings ON repo_memory_bindings.memory_id = repo_memories.id
        LEFT JOIN chunks ON chunks.id = ?1
        LEFT JOIN files ON files.id = chunks.file_id
        WHERE repo_memories.status IN ('active', 'stale'){repo_clause}
          AND (
              repo_memory_bindings.chunk_id = ?1
              OR (files.path IS NOT NULL AND repo_memory_bindings.path = files.path)
          )
        ORDER BY repo_memories.updated_at_ms DESC
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
pub(crate) fn memories_for_path(
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
pub(crate) fn memories_for_symbol(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
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
        if let Some(memory) = memory_by_id(conn, &id)? {
            memories.push(memory);
        }
    }
    memories.sort_by_key(|memory| std::cmp::Reverse(memory.updated_at_ms));
    Ok(memories)
}
pub fn memory_evidence_for_symbol(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
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
pub(crate) fn memory_evidence_for_symbol_and_edges(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
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
pub(crate) fn memories_for_edges(
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
pub(crate) fn memories_for_call_path_hash(
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
pub(crate) fn memory_search(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<RepoMemory>> {
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
    let mut stmt = conn.prepare(&format!(
        "
        SELECT DISTINCT repo_memory_fts.memory_id
        FROM repo_memory_fts
        JOIN repo_memories ON repo_memories.id = repo_memory_fts.memory_id
        WHERE repo_memory_fts MATCH ?1
          AND repo_memories.status IN ('active', 'stale'){repo_clause}
        ORDER BY bm25(repo_memory_fts)
        LIMIT ?2
        "
    ))?;
    ids_to_memories(
        conn,
        stmt.query_map(params![query, i64::from(limit)], |row| row.get("memory_id"))?,
    )
}

pub(crate) fn rebind_memory(
    conn: &Connection,
    memory_id: &str,
    bind: RepoMemoryBindTarget,
) -> anyhow::Result<RepoMemory> {
    if memory_by_id(conn, memory_id)?.is_none() {
        anyhow::bail!("memory `{memory_id}` not found");
    }
    // Resolve inside the transaction so the stamped source_text_hash is consistent with the
    // bindings written in the same atomic unit.
    // Authored write (#560): a rebind is an explicit, non-reconstructable choice of a new anchor,
    // so it commits durably even though it does not yet mint a signed op (it will ride the FULL
    // path for free once it does). NORMAL restored on drop.
    let _durability = authoring::AuthoredDurability::begin(conn)?;
    let tx = conn.unchecked_transaction()?;
    // Rebind MUST name an anchor — moving a memory to "no binding" is meaningless (delete/recreate
    // it unanchored instead). Only `create_memory` accepts the unanchored (`None`) case (#463).
    let binding = resolve_binding(conn, &bind)?.ok_or_else(|| {
        anyhow::anyhow!(
            "memory_rebind requires a binding target: logical_symbol_id, symbol_id, chunk_id, \
             edge_id, call path, path/span, commit_hash, or tracker ref"
        )
    })?;
    conn.execute("DELETE FROM repo_memory_bindings WHERE memory_id = ?1", [memory_id])?;
    conn.execute("DELETE FROM repo_memory_call_paths WHERE memory_id = ?1", [memory_id])?;
    let now = now_ms();
    insert_binding(conn, memory_id, &binding, now)?;
    insert_auto_moniker_binding(conn, memory_id, &binding, now)?;
    // Re-stamp the freshly re-inserted bindings from the parent memory's repo (the create path does
    // this too). Without it the rebound rows keep the `__unassigned__` default and fall out of the
    // binding-scoped reads while the parent memory stays in its real repo — the review finding.
    stamp_bindings_from_parent_repo(conn, memory_id)?;
    conn.execute(
        "UPDATE repo_memories SET source_text_hash = ?2, updated_at_ms = ?3 WHERE id = ?1",
        params![memory_id, binding.source_text_hash, now],
    )?;
    tx.commit()?;
    memory_by_id(conn, memory_id)?
        .ok_or_else(|| anyhow::anyhow!("rebound memory `{memory_id}` could not be read back"))
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
pub(crate) fn list_memories(
    conn: &Connection,
    kind: Option<&str>,
) -> anyhow::Result<Vec<MemorySummary>> {
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
            JOIN repo_memory_bindings AS b ON b.memory_id = m.id
            WHERE m.status IN ('active', 'stale'){repo_clause}
              AND b.binding_kind = ?1
              AND b.rowid = (
                  SELECT b2.rowid FROM repo_memory_bindings AS b2
                  WHERE b2.memory_id = m.id
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
              ON b.memory_id = m.id
              AND b.rowid = (
                  SELECT b2.rowid FROM repo_memory_bindings AS b2
                  WHERE b2.memory_id = m.id
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
pub(crate) fn doctor_attention_count(conn: &Connection) -> anyhow::Result<u64> {
    let scope = memory_repo_scope(conn)?;
    let repo_clause = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "m");
    let count: i64 = conn.query_row(
        &format!(
            "
        SELECT COUNT(*)
        FROM repo_memory_bindings AS b
        JOIN repo_memories AS m ON m.id = b.memory_id
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
         JOIN repo_memories AS m ON m.id = b.memory_id
         WHERE m.status = 'active'
           AND b.anchor_status IN ('gone', 'stale')
           AND b.binding_kind != 'scip_moniker'{repo_clause}
         ORDER BY b.memory_id"
    ))?
    .query_map([], |row| row.get::<_, String>(0))?
    .collect()
}

pub(crate) fn doctor_report(conn: &Connection) -> anyhow::Result<Vec<MemoryDoctorEntry>> {
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
        JOIN repo_memories AS m ON m.id = b.memory_id
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
pub(crate) fn anchor_health_counts(
    conn: &Connection,
) -> anyhow::Result<crate::index::AnchorHealth> {
    let mut health = crate::index::AnchorHealth::default();
    // Scoped to the active repo (V042): these counts drive per-repo doctor warnings, so a sibling
    // repo's bindings must not inflate them on a consolidated DB.
    let scope = memory_repo_scope(conn)?;
    let repo_clause = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "m");
    let mut stmt = conn.prepare(&format!(
        "
        SELECT b.anchor_status, COUNT(*) AS cnt
        FROM repo_memory_bindings AS b
        JOIN repo_memories AS m ON m.id = b.memory_id
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

pub(crate) fn validate_memories(
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
        report.checked += 1;
        let status = validate_binding(conn, &mut binding, fs_root)?;
        // Downgrade hysteresis (#492): a single `gone` observation of a not-yet-gone binding is
        // exactly what a torn pass produces (a validate racing a rebuild window, or a sweep from
        // a checkout context that cannot see the anchor another context re-asserts), and a
        // persisted `gone` is what doctor turns into destructive mark-obsolete advice. So a
        // FIRST gone observation only ARMS `downgrade_pending_at_ms` — the stored row stays as
        // it was — and only a SECOND consecutive one persists the downgrade. Any non-gone stamp
        // clears the marker (the ping-pong never lands), and a staged-generation window freezes
        // the rule entirely (see `staged_window` above). The REPORT keeps counting the computed
        // observation: what this pass saw is honest; only what doctor reads is hysteresis-
        // guarded.
        if status == "gone" && stored_status != "gone" {
            if staged_window {
                // Untrustworthy observation: leave the row exactly as it was.
            } else if downgrade_pending_at_ms.is_none() {
                conn.execute(
                    "UPDATE repo_memory_bindings SET downgrade_pending_at_ms = ?4
                     WHERE memory_id = ?1 AND binding_kind = ?2 AND binding_id = ?3",
                    params![binding.memory_id, binding.binding_kind, original_binding_id, now_ms()],
                )?;
            } else {
                stamp_validated_binding(conn, &binding, &original_binding_id, &status)?;
            }
        } else {
            stamp_validated_binding(conn, &binding, &original_binding_id, &status)?;
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
    tx.commit()?;
    Ok(report)
}

/// Persist one validated binding: the full field rewrite `validate_memories` stamps for every
/// non-deferred observation. Clears `downgrade_pending_at_ms` — a persisted stamp is either an
/// upgrade (the anchor is seen again; the pending downgrade is disarmed) or the confirmed
/// downgrade itself (the marker's job is done).
fn stamp_validated_binding(
    conn: &Connection,
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
            binding.relocation_reason
        ],
    )?;
    // UPDATE OR IGNORE: if a sibling binding already holds the new (memory_id, kind,
    // binding_id) PK, the rewrite is a no-op rather than a crash. Drop the
    // now-duplicate stale row.
    if updated == 0 && binding.binding_id != original_binding_id {
        conn.execute(
            "DELETE FROM repo_memory_bindings
             WHERE memory_id = ?1 AND binding_kind = ?2 AND binding_id = ?3",
            params![binding.memory_id, binding.binding_kind, original_binding_id],
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
