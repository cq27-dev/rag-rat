//! The four AUTHORED memory mutations. Each one authors its op-log entry (or takes the authored
//! durability posture) inside the same transaction as the row write — an authoring error rolls
//! the mutation back. Read-side machinery (validation of kinds/confidence, binding resolution,
//! hydration) comes from `rag_rat_query::memory`.

use rag_rat_base::time::now_ms;
use rag_rat_query::memory::{
    MAX_MEMORY_BODY_LEN, MAX_MEMORY_TITLE_LEN, RepoMemory, RepoMemoryBindTarget, RepoMemoryCreate,
    RepoMemoryCreateResult, RepoMemoryUpdate, duplicate_memory_id, insert_auto_moniker_binding,
    insert_binding, is_polymorphic_node_kind, memory_by_id, memory_id, memory_input_hash,
    memory_repo_scope, normalize_tags, replace_tags, resolve_binding,
    stamp_bindings_from_parent_repo, upsert_memory_fts, validate_confidence, validate_kind,
    validate_len, validate_payload, validate_source, validate_status,
};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::authoring;

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
    let prepared = authoring::prepare_live_content_authoring(conn, now)?;
    // Authored write: commit durably so a `memory_create` that returned success survives power loss
    // (#560). FULL for this transaction only; the connection restores NORMAL on drop.
    let _durability = authoring::AuthoredDurability::begin(conn)?;
    // IMMEDIATE, not deferred: memory writes are the sanctioned flock-less writers on the shared
    // database, racing foreign repos' rebuilds by design. A deferred txn that READS first (the
    // tombstone check below) and then upgrades to write fails with SQLITE_BUSY_SNAPSHOT the moment
    // a concurrent writer committed in between — and that error BYPASSES the busy handler, so
    // busy_timeout never gets a say. Taking the write lock up front waits it out instead (#818).
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // #767: revalidate the removal tombstone INSIDE the write txn — a connection that resolved its
    // active scope before `rm` ran must fail closed here rather than stamp the removed `repo_id`
    // onto a fresh row after the purge.
    if let Some(repo_id) = &scope {
        super::assert_repo_not_removed(conn, repo_id)?;
    }
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
    authoring::author_create(&tx, &memory, prepared.as_ref(), now)?;
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
    let prepared = authoring::prepare_live_content_authoring(conn, now)?;
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
    authoring::author_update(
        &tx,
        &memory,
        content_changed,
        status_changed,
        prepared.as_ref(),
        now,
    )?;
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
    // IMMEDIATE for the same reason as `create_memory`: `resolve_binding` READS before the writes
    // below, and a deferred read→write upgrade racing a foreign repo's rebuild dies with
    // SQLITE_BUSY_SNAPSHOT, which the busy handler never sees.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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

#[cfg(test)]
mod tests {
    use rag_rat_query::memory::{EdgeRelation, EdgeTarget, RepoMemoryBindTarget, RepoMemoryCreate};
    use rusqlite::Connection;

    use super::{create_memory, rebind_memory};
    use crate::memory_write::add_edge;

    const REPO: &str = "repo-a";

    /// A DB with the memory schema, one registered repo, and the connection scoped to it — the
    /// minimal setup `memory_repo_scope` needs to resolve an active repo. Mirrors
    /// `authoring::tests::scoped_conn` (each module's test scaffolding is self-contained).
    fn scoped_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        rag_rat_db::schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?1, 0)",
            [REPO],
        )
        .unwrap();
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

    /// Create an unanchored `Concept` (needs no code binding) through the LIVE `create_memory`.
    fn create_concept(conn: &Connection, title: &str) -> anyhow::Result<String> {
        Ok(create_memory(conn, RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: title.to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: None,
            bind: RepoMemoryBindTarget::default(),
        })?
        .memory
        .memory_id)
    }

    /// #767 review: a connection that resolved its active repo scope BEFORE `rag-rat rm` ran must
    /// fail closed at write time — the removal tombstone is revalidated inside the write
    /// transaction, so a post-purge `create_memory` cannot stamp the removed `repo_id` onto a fresh
    /// row (and its op-log entry) after `rm` reported success.
    #[test]
    fn create_memory_refuses_a_tombstoned_repo_until_it_is_cleared() {
        let conn = scoped_conn();
        create_concept(&conn, "before removal").unwrap();

        rag_rat_db::schema::mark_repo_removed(&conn, REPO, 1).unwrap();
        let err = create_concept(&conn, "after removal")
            .expect_err("a tombstoned repo must refuse a memory create");
        assert!(
            err.to_string().contains("rag-rat rm"),
            "the refusal must name the removal remedy, got: {err}"
        );

        rag_rat_db::schema::clear_repo_removed(&conn, REPO).unwrap();
        create_concept(&conn, "after re-add").unwrap();
    }

    /// The same gate covers the edge INSERT path: `add_edge` stamps the source node's owner
    /// `repo_id` onto the new row, so it revalidates the tombstone in-transaction too.
    #[test]
    fn add_edge_refuses_a_tombstoned_repo() {
        let conn = scoped_conn();
        let source = create_concept(&conn, "source").unwrap();
        let target = create_concept(&conn, "target").unwrap();

        rag_rat_db::schema::mark_repo_removed(&conn, REPO, 1).unwrap();
        let err = add_edge(&conn, &source, EdgeRelation::RelatesTo, &EdgeTarget::Node {
            repo_id: None,
            node_id: target,
        })
        .expect_err("a tombstoned repo must refuse an edge add");
        assert!(
            err.to_string().contains("rag-rat rm"),
            "the refusal must name the removal remedy, got: {err}"
        );
    }

    /// A NON-insert mutation is unaffected by the gate: `rebind_memory` writes no new repo-stamped
    /// rows (and post-purge it has no row to find anyway), so it must not trip the tombstone check
    /// on a still-populated store.
    #[test]
    fn rebind_memory_is_not_blocked_by_the_tombstone_gate() {
        let conn = scoped_conn();
        let id = create_concept(&conn, "rebind me").unwrap();

        rag_rat_db::schema::mark_repo_removed(&conn, REPO, 1).unwrap();
        rebind_memory(&conn, &id, RepoMemoryBindTarget {
            path: Some("src/lib.rs".to_string()),
            ..Default::default()
        })
        .unwrap();
    }
}
