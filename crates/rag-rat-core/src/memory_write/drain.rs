//! Draining accepted `/3` content into `repo_memories` / `repo_node_edges` — the REVERSE of the
//! local reconcile (`authoring::reconcile_owner_stream_for_repo`, which authors local rows INTO the
//! signed `/3` log). Here, a stream's accepted projection is mirrored back OUT into the local
//! memory tables as `origin='synced'` rows, so a memory authored on one device becomes a real,
//! searchable local row on another device of the same account.
//!
//! REPO ATTRIBUTION BY FORWARD DERIVATION. Content `/3` is one stream per `(repo_id, account_id)`,
//! and every device of one account shares the account id, so the owner stream id is identical on
//! every device. Given a local `repo_id` the drain therefore FORWARD-derives the stream and mirrors
//! exactly that stream's projection — the same input the local reconcile takes.
//!
//! ONE AUTHORITATIVE STREAM PER REPO. Which stream that is comes from
//! [`authoritative_content_stream`] and there is never more than one: the removal anti-joins read
//! "absent from this stream's projection" as "condemned", so a second stream materializing into the
//! same repo would delete the first's rows. A granted contributor (#1164) therefore REPLACES its
//! local derivation with the configured owner's stream rather than draining both.
//!
//! CONVERGE, DON'T FREEZE. The accepted `/3` projection is the LWW-merged content across ALL of the
//! account's devices, INCLUDING this one — so when another device updates or removes a memory/edge
//! this device originally created, the projection holds the winning value and the drain must
//! CONVERGE the local row to it, preserving the row's `origin` (a row created here stays `'local'`,
//! a row received from a peer stays `'synced'`). Skipping a projected `origin='local'` row would
//! freeze this device on stale content forever. The one thing the drain must NOT touch is a local
//! row that is ABSENT from the projection — that is a pending local edit not yet reconciled into
//! the log, so it is left alone (and, being `origin='local'`, is spared by the synced-only removal
//! anti-joins). New `origin` on WRITE: an absent row is INSERTed `'synced'` (received from a peer);
//! an existing row keeps its own origin.
//!
//! NO ECHO. Convergence never re-authors: the authoring-side anti-join
//! (`read_unauthored_memory_rows` / `unauthored_edges`, `WHERE origin='local' AND NOT EXISTS
//! (…projection…)`) re-authors only a local row MISSING from the projection. A converged local row
//! IS in the projection, so it is never in the unauthored set — the round-trip cannot loop.
//!
//! CROSS-REPO BOUNDARY + ROBUSTNESS. Projected content is peer-authored and only shape-validated,
//! so the drain treats it as untrusted at the repo boundary: a node id already owned by ANOTHER
//! repo is left untouched (node id is a global PK — a peer must never overwrite a sibling's row),
//! an edge whose self-declared `owner_repo_id` is not this repo is skipped (never injected into /
//! removed from a sibling), and an edge only materializes when its source node is a row THIS repo
//! owns — an absent source (retro-condemned away) would abort the whole drain on its
//! `source_node_id` FK (wedging every open), and a source id colliding with a sibling's node would
//! attach a this-repo edge to that sibling's row.
//!
//! PER-DEVICE STATE IS NOT CONVERGED. An edge's resolution triple (`target_repo_id resolution`,
//! `target_node_id`, `anchor_status`) is per-device derived state recomputed on read
//! (`reresolve_on_read`), never converged: the drain writes the DURABLE spec (owner + signed
//! `target_repo_id`) but stores the resolution `unresolved` on INSERT and never imports a peer's
//! projected `Rebind` anchor (that would splice one device's resolution into another's edge), and
//! it never rewrites the resolution on a converge (that would wipe a resolved local edge every
//! pass). The read path recomputes the whole triple locally.
//!
//! ATOMICITY. The projection read + the table writes run inside ONE `IMMEDIATE` transaction, after
//! settling any pending refold for the stream (the same fail-closed barrier the reconcile uses) so
//! the projection is current. The synced rows are fully reconstructable from the durable `/3` log,
//! so — unlike an authored write — the drain does NOT raise `synchronous = FULL`; a lost drain
//! simply re-materializes on the next pass.

use rag_rat_oplog::{self, ProjectedContentEdge, ProjectedContentNode, StreamId};
use rag_rat_query::memory;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

/// The `repo_memories.memory_version` a synced row is stamped with — the author-side constant the
/// live create path mints (`'v1'`), so a drained row is indistinguishable from a locally-authored
/// one in everything but `origin`. Kept in lock-step with `api::create_memory`.
const SYNCED_MEMORY_VERSION: &str = "v1";

/// What one drain pass changed. Owned + flat counts; a re-drain over an unchanged projection
/// reports all-zero (the idempotence contract).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DrainOutcome {
    /// Nodes inserted (synced) or converged (content/status/tags updated to the projection).
    pub nodes_written: u32,
    /// Synced nodes removed because their projection row vanished (retro-condemn / revocation). A
    /// local row absent from the projection is a pending edit and is NOT counted or removed.
    pub nodes_removed: u32,
    /// Edges inserted (synced) or converged (durable spec updated to the projection).
    pub edges_written: u32,
    /// Edges removed — a `present=0` tombstone (regardless of origin: a peer's remove wins), or a
    /// synced edge whose projection row vanished entirely (retro-condemn).
    pub edges_removed: u32,
}

impl DrainOutcome {
    fn add(&mut self, other: DrainOutcome) {
        self.nodes_written += other.nodes_written;
        self.nodes_removed += other.nodes_removed;
        self.edges_written += other.edges_written;
        self.edges_removed += other.edges_removed;
    }
}

/// The ONE stream that materializes `repo_id`'s synced rows. EXACTLY ONE — every removal anti-join
/// below reads "absent from this stream's projection" as "condemned", and the drain watermark is
/// per-stream, so two streams materializing into the same repo would delete each other's rows and
/// then not restore them (each stream's watermark says it is up to date). So contribution mode
/// (#1164) does not ADD the owner's stream to this repo's drain, it REPLACES the local one: a
/// granted contributor authors nothing onto its own owner stream, which would sit empty and
/// condemn everything the owner's stream materialized.
///
/// Scope-gated the same way the reconcile is — a LEGACY placeholder or a `local:` shallow-clone id
/// can never root an owner stream, so both yield `None`.
fn authoritative_content_stream(
    conn: &Connection,
    repo_id: &str,
) -> anyhow::Result<Option<StreamId>> {
    // Only a STABLE id derives an immutable owner stream (mirrors `sync_owner_stream`): the legacy
    // `__unassigned__` placeholder and a `local:` shallow-clone id both get re-pointed later, so a
    // stream derived under them would strand. No synced content can exist for such an id anyway.
    if repo_id == rag_rat_base::repo_identity::LEGACY_REPO_ID
        || repo_id.starts_with(rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX)
    {
        return Ok(None);
    }
    // A granted CONTRIBUTOR reads back the CONFIGURED owner's stream — where its own writes went
    // and where the owner's and other contributors' memories live. A configured owner that IS this
    // store is not contribution mode; fall through to the local derivation.
    if let Some(owner) = super::authoring::contribution_owner_account(conn, repo_id)?
        && rag_rat_oplog::read_local_account(conn)? != Some(owner)
    {
        // Contribution targets the owner's PublicRead stream (v1 public only).
        let stream = rag_rat_oplog::owner_stream_v2_id_for_account(
            repo_id,
            owner,
            rag_rat_oplog::AccessMode::PublicRead,
        )?;
        // A DERIVED stream id is not yet an authority. `sync contribute` deliberately succeeds
        // before the owner's log is synced (configure, then sync), and a mistyped owner id derives
        // a stream that will never exist at all — in both cases the projection is EMPTY, and
        // handing that to the drain would make the removal anti-joins condemn every synced row the
        // repo currently reads. So authority begins only once the ownership fact has folded here.
        // Until then this repo drains NOTHING: `None` rather than falling through to the local
        // stream, whose own empty projection would condemn exactly the same rows.
        if rag_rat_oplog::stream_owner_account(conn, stream)? != Some(owner) {
            return Ok(None);
        }
        return Ok(Some(stream));
    }
    // Forward-derive the owner stream under the repo's access-mode intent — the SAME stream id the
    // live-write authored onto, so a published (PublicRead) repo drains its own public stream
    // rather than an empty Private one. `None` = no local account minted yet ⇒ nothing could
    // have been authored/ingested onto this stream ⇒ nothing to drain (the analog of an
    // unstable scope).
    let mode = super::authoring::owner_stream_access_mode(conn, repo_id)?;
    rag_rat_oplog::owned_stream_v2_id_with_mode(conn, repo_id, mode)
}

/// Mirror a repo's accepted synced `/3` content into its local memory tables, from the one stream
/// [`authoritative_content_stream`] names. Scope-EXPLICIT (the `repo_id` is passed). Opens its own
/// `IMMEDIATE` transaction, settles that stream's pending refold inside it (fail-closed), then
/// drains.
pub(crate) fn drain_synced_stream_for_repo(
    conn: &Connection,
    repo_id: &str,
    now_ms: i64,
) -> anyhow::Result<DrainOutcome> {
    let Some(stream) = authoritative_content_stream(conn, repo_id)? else {
        return Ok(DrainOutcome::default());
    };
    // Cheap read-only gate: skip the write txn + O(projection) scan entirely when the projection is
    // unchanged since this stream was last drained and nothing is pending. Every drain seam routes
    // through here (open, consolidate, and the long-running watcher pass), so all three go O(1)
    // when idle instead of O(projection); the first-ever drain has no watermark and always runs
    // (the backfill). Read-only and outside the txn, so a concurrent author that advances the
    // epoch right after this check is simply picked up by the next drain — delayed one pass,
    // never lost.
    if !rag_rat_oplog::content_drain_needed(conn, stream)? {
        return Ok(DrainOutcome::default());
    }

    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Re-check the removal tombstone INSIDE the write txn (#767).
    // `drain_synced_streams_for_all_repos` snapshots `real_repo_ids` outside any txn, so a
    // concurrent `rag-rat rm` can purge this repo and commit its removal marker between that
    // snapshot and here. Both are IMMEDIATE txns and serialize: if `rm` took the lock first,
    // its tombstone is visible now — materializing would RESURRECT the synced rows `rm` just
    // purged and reported gone. Skip a removed repo before any write.
    if rag_rat_db::schema::is_repo_removed(&tx, repo_id)? {
        return Ok(DrainOutcome::default());
    }
    // Settle the owner stream's deferred refold HERE, inside the write's own transaction and before
    // the projection read, so the drain mirrors a CURRENT accepted set. A settle failure propagates
    // and rolls the drain back, so the barrier stays fail-closed (same discipline as the
    // reconcile).
    rag_rat_oplog::settle_pending_content_refold_for_stream_in_tx(&tx, stream, now_ms)?;
    let outcome = drain_synced_stream_in_tx(&tx, repo_id, stream, now_ms)?;
    // Stamp the watermark to the epoch AFTER the settle above (which may have advanced it), so the
    // next `content_drain_needed` short-circuits until the projection changes again. In the same
    // txn as the scan, so a rolled-back drain never records progress it did not make.
    rag_rat_oplog::record_content_drained(&tx, stream)?;
    tx.commit()?;
    Ok(outcome)
}

/// Drain every registered real repo's synced stream — the store-global counterpart wired into the
/// open/migrate seam, after the projection is rebuilt current. Per-repo derivation means a repo
/// with no minted account (or no synced content) is a cheap no-op, so this stays light on a plain
/// open.
pub(crate) fn drain_synced_streams_for_all_repos(
    conn: &Connection,
    now_ms: i64,
) -> anyhow::Result<DrainOutcome> {
    let mut total = DrainOutcome::default();
    for repo_id in rag_rat_db::schema::real_repo_ids(conn)? {
        total.add(drain_synced_stream_for_repo(conn, &repo_id, now_ms)?);
    }
    Ok(total)
}

/// The in-transaction drain worker: NODES first (so an edge's `source_node_id` FK target exists),
/// then edges, then the two retro-condemn removals. Reads the projection through the oplog decode
/// helpers; CONVERGES each projected row into the local tables (INSERT synced if absent, else
/// update preserving origin) and removes rows the projection dropped. Assumes the caller settled
/// any pending refold so the projection is current.
fn drain_synced_stream_in_tx(
    tx: &Transaction<'_>,
    repo_id: &str,
    stream: StreamId,
    now_ms: i64,
) -> anyhow::Result<DrainOutcome> {
    let mut outcome = DrainOutcome::default();

    // (1) Nodes: converge every projected node into the local tables (INSERT synced if absent,
    // else update content/status/tags preserving origin), so every edge's source node exists
    // before the edge pass.
    for node in rag_rat_oplog::list_projected_content_nodes(tx, stream)? {
        match drain_node(tx, repo_id, &node, now_ms)? {
            NodeEffect::Written => outcome.nodes_written += 1,
            NodeEffect::Removed => outcome.nodes_removed += 1,
            NodeEffect::Unchanged => {},
        }
    }

    // (2) Edges: a present edge converges (INSERT synced / update durable spec), a `present=0`
    // tombstone removes the edge regardless of origin (a peer's remove wins).
    for edge in rag_rat_oplog::list_projected_content_edges(tx, stream)? {
        match drain_edge(tx, repo_id, &edge, now_ms)? {
            EdgeEffect::Written => outcome.edges_written += 1,
            EdgeEffect::Removed => outcome.edges_removed += 1,
            EdgeEffect::Unchanged => {},
        }
    }

    // (3) Retro-condemn: a synced edge whose projection row VANISHED entirely (not a present=0
    // tombstone, which is still IN the projection) is removed — only `origin='synced'` rows.
    outcome.edges_removed += remove_vanished_synced_edges(tx, repo_id, stream)?;

    // (4) Retro-condemn: a synced NODE whose projection row vanished is removed. An
    // `origin='local'` row of the same id (a genuine local ghost the reconcile will author) is
    // left intact by the origin gate. Runs last so the edge passes above still saw their FK
    // targets.
    outcome.nodes_removed += remove_vanished_synced_nodes(tx, repo_id, stream)?;

    Ok(outcome)
}

/// One existing `repo_memories` row's convergence-relevant columns. `repo_id` gates the converge to
/// OUR repo (node id is a global PK, so a peer stream naming an id another repo already owns must
/// NOT be allowed to overwrite that sibling); `origin` is deliberately NOT read — a converge
/// preserves whatever it is, it is never a gate.
struct ExistingNode {
    repo_id: String,
    kind: String,
    title: String,
    body: String,
    confidence: String,
    source: String,
    payload_json: Option<String>,
    status: String,
}

fn read_existing_node(conn: &Connection, node_id: &str) -> anyhow::Result<Option<ExistingNode>> {
    conn.query_row(
        "SELECT repo_id, kind, title, body, confidence, source, payload_json, status
         FROM repo_memories WHERE id = ?1",
        [node_id],
        |row| {
            Ok(ExistingNode {
                repo_id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                body: row.get(3)?,
                confidence: row.get(4)?,
                source: row.get(5)?,
                payload_json: row.get(6)?,
                status: row.get(7)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Whether a projected node's content passes the SAME validity gates the local create/update path
/// enforces — kind / confidence closed sets, title / body length caps, source, and the kind↔payload
/// rule (`validate_payload`). Peer content crosses the wire only SHAPE-validated, and an older or
/// compromised account device could author content that clears the §18a envelope cap yet violates
/// these tighter local rules; such a node must not be persisted into the searchable tables.
fn projected_node_content_is_valid(node: &ProjectedContentNode) -> anyhow::Result<()> {
    memory::validate_kind(&node.content.kind)?;
    memory::validate_confidence(&node.content.confidence)?;
    memory::validate_source(&node.content.source)?;
    memory::validate_len("title", &node.content.title, memory::MAX_MEMORY_TITLE_LEN)?;
    memory::validate_len("body", &node.content.body, memory::MAX_MEMORY_BODY_LEN)?;
    memory::validate_payload(&node.content.kind, node.content.payload.as_deref())?;
    // Tags cross the same untrusted boundary. The local write path (`replace_tags`) caps each
    // NORMALIZED tag at 64 bytes; an over-cap tag would otherwise error inside
    // `write_node_children` and roll back the WHOLE drain (wedging every subsequent open on the
    // same accepted projection). Validate the normalized tags here so an oversized tag is
    // quarantined like any other field.
    for tag in memory::normalize_tags(&node.content.tags) {
        memory::validate_len("tag", &tag, 64)?;
    }
    Ok(())
}

/// What a node converge did — mirrors [`EdgeEffect`] so the outcome counters stay honest (a
/// quarantine that drops a stale synced mirror is a removal, not a write).
enum NodeEffect {
    Written,
    Removed,
    Unchanged,
}

/// Converge one projected node into `repo_memories`. Absent locally → INSERT `origin='synced'` (a
/// row received from a peer). Present under OUR repo → UPDATE its content/status/tags to the
/// projection value, PRESERVING the existing `origin` (a locally-authored row stays `'local'`) —
/// the projection is the account-wide LWW winner, so a projected local row carries another device's
/// accepted edit and must converge, not freeze. Present under a DIFFERENT repo → skip (node id is a
/// global PK; a peer stream must never overwrite a sibling repo's row). A local row ABSENT from the
/// projection is never seen here (the loop iterates projected rows) and is left untouched as a
/// pending local edit. Content that fails the local validity gates is QUARANTINED (skipped +
/// warned, never persisted and never wedging the drain — symmetric to the authoring-side #680
/// quarantine) — and if a prior synced mirror of that id exists, the stale row is REMOVED so it
/// stops being searchable. Returns the [`NodeEffect`]: a converge INSERT/UPDATE is `Written`, a
/// quarantine that drops a stale synced mirror is `Removed`, and a no-op (a row already equal to
/// the projection, a sibling-owned id, or invalid content with nothing to remove) is `Unchanged` —
/// the idempotence contract.
fn drain_node(
    tx: &Transaction<'_>,
    repo_id: &str,
    node: &ProjectedContentNode,
    now_ms: i64,
) -> anyhow::Result<NodeEffect> {
    // Quarantine invalid peer content rather than persist a malformed row or wedge the whole drain.
    if let Err(err) = projected_node_content_is_valid(node) {
        tracing::warn!(
            repo_id,
            node_id = %node.node_id,
            error = %err,
            "quarantining an invalid synced memory node: its content violates a local rule the \
             create/update path enforces (kind/confidence/length/payload); skipped, not persisted",
        );
        // If this id already has a materialized synced mirror, the accepted (now-invalid) value
        // supersedes it: we cannot persist the invalid content, but leaving the STALE prior row
        // searchable would expose a value the projection no longer holds. Drop the synced mirror (a
        // local row of the same id survives).
        let removed = remove_quarantined_synced_node(tx, repo_id, &node.node_id)?;
        return Ok(if removed { NodeEffect::Removed } else { NodeEffect::Unchanged });
    }
    let projected_status = node.status.as_db_str();
    let effect = match read_existing_node(tx, &node.node_id)? {
        // A row with this id already belongs to ANOTHER repo — never touch a sibling's content.
        Some(existing) if existing.repo_id != repo_id => NodeEffect::Unchanged,
        Some(existing) => {
            let current_tags = memory::tags_for_memory(tx, &node.node_id)?;
            let want_tags = memory::normalize_tags(&node.content.tags);
            let unchanged = existing.kind == node.content.kind
                && existing.title == node.content.title
                && existing.body == node.content.body
                && existing.confidence == node.content.confidence
                && existing.source == node.content.source
                && existing.payload_json.as_deref() == node.content.payload.as_deref()
                && existing.status == projected_status
                && current_tags == want_tags;
            if unchanged {
                // Content converged, but the anchor snapshot may have arrived in a later entry, so
                // fall through to the seed rather than returning here.
                NodeEffect::Unchanged
            } else {
                // Converge to the account-wide LWW value; `origin` is intentionally NOT in the SET,
                // so whatever the row was (local or synced) is preserved — a
                // projected local row carries a peer's accepted edit, not an echo
                // to ignore.
                tx.execute(
                    "UPDATE repo_memories
                     SET kind = ?2, title = ?3, body = ?4, confidence = ?5, source = ?6,
                         payload_json = ?7, status = ?8, updated_at_ms = ?9
                     WHERE id = ?1",
                    params![
                        node.node_id,
                        node.content.kind,
                        node.content.title,
                        node.content.body,
                        node.content.confidence,
                        node.content.source,
                        node.content.payload,
                        projected_status,
                        now_ms,
                    ],
                )?;
                write_node_children(tx, &node.node_id, &node.content.tags)?;
                NodeEffect::Written
            }
        },
        None => {
            // First sight of this node — received from a peer. `created_by` / `source_text_hash` /
            // `input_hash` have no op home and are nullable; `memory_version` is the author-side
            // constant; the clock is bookkeeping only. `origin='synced'` is what the authoring gate
            // keys off.
            tx.execute(
                "INSERT INTO repo_memories(
                     id, kind, title, body, confidence, status, created_by, created_at_ms,
                     updated_at_ms, source, payload_json, source_text_hash, input_hash,
                     memory_version, repo_id, origin)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?7, ?8, ?9, NULL, NULL, ?10, ?11,
                     'synced')",
                params![
                    node.node_id,
                    node.content.kind,
                    node.content.title,
                    node.content.body,
                    node.content.confidence,
                    projected_status,
                    now_ms,
                    node.content.source,
                    node.content.payload,
                    SYNCED_MEMORY_VERSION,
                    repo_id,
                ],
            )?;
            write_node_children(tx, &node.node_id, &node.content.tags)?;
            NodeEffect::Written
        },
    };
    // Seed AFTER materialization, and only for a node that belongs to THIS repo — the sibling-repo
    // arm above must not touch that repo's bindings any more than it touches its content.
    if node_in_repo(tx, &node.node_id, repo_id)? {
        seed_node_anchors(tx, repo_id, node)?;
    }
    Ok(effect)
}

/// The binding kinds the local write path can produce, and therefore the only ones worth seeding
/// from a peer's snapshot. An unknown kind is a newer peer's vocabulary: it means nothing here, so
/// it is skipped row-wise rather than quarantining the node over its decoration.
///
/// `call_path` is deliberately absent even though the local path produces it: its supporting
/// `repo_memory_call_paths` / `_edges` rows are in NO replication scope, so a seeded call-path
/// binding could never resolve here — it would sit unverifiable forever.
const SEEDABLE_BINDING_KINDS: &[&str] = &[
    "logical_symbol",
    "symbol",
    "chunk",
    "edge",
    "scip_moniker",
    "path",
    "dir",
    "commit",
    "tracker",
];

/// Seed a synced memory's bindings from the anchor snapshot its author published — ONLY when this
/// store holds none for it.
///
/// The gate is what keeps this a fallback rather than a second writer. `anchors/1` remains the
/// carrier of ongoing rebinds and relocations within an account; this writes into vacuum exactly
/// once, so the two never contend for a live row and there is no clock to arbitrate.
///
/// It is per-MEMORY, not per-row: the validate/relocate loop re-keys `binding_id`, a PK column, so
/// a per-row gate would look at a row the loop had moved, find its old identity absent, and
/// resurrect it as a duplicate sibling — forever.
///
/// `None` anchors means nobody published this memory's bindings, which is NOT the same as an author
/// publishing an empty set; only the latter is a statement, and neither seeds anything.
fn seed_node_anchors(
    tx: &Transaction<'_>,
    repo_id: &str,
    node: &ProjectedContentNode,
) -> anyhow::Result<usize> {
    let Some(anchors) = node.anchors.as_deref() else {
        return Ok(0);
    };
    if anchors.is_empty() || memory_has_any_binding(tx, repo_id, &node.node_id)? {
        return Ok(0);
    }
    let mut seeded = 0;
    for anchor in anchors {
        if !SEEDABLE_BINDING_KINDS.contains(&anchor.binding_kind.as_str())
            || anchor.binding_id.is_empty()
        {
            tracing::warn!(
                repo_id,
                node_id = %node.node_id,
                binding_kind = %anchor.binding_kind,
                "skipping an anchor this store cannot seed: unknown binding kind, or an empty \
                 binding id that would make a degenerate primary key",
            );
            continue;
        }
        // Portable columns only. Every checkout-local column — `anchor_status`, the resolved ids,
        // the relocation bookkeeping — is left at its schema default, which is exactly the row
        // state a `/5` apply produces, so the validate/relocate loop takes it from here with
        // nothing special-cased for a seeded row.
        tx.execute(
            "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                 commit_hash, tracker, project, item_key, created_at_ms, symbol_kind,
                 signature_hash, moniker_tool, moniker_tool_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                repo_id,
                node.node_id,
                anchor.binding_kind,
                anchor.binding_id,
                anchor.path,
                anchor.start_line,
                anchor.end_line,
                anchor.commit_hash,
                anchor.tracker,
                anchor.project,
                anchor.item_key,
                anchor.created_at_ms,
                anchor.symbol_kind,
                anchor.signature_hash,
                anchor.moniker_tool,
                anchor.moniker_tool_version,
            ],
        )?;
        seeded += 1;
    }
    Ok(seeded)
}

/// Whether this store holds ANY binding for `(repo_id, memory_id)` — the per-memory seed gate.
fn memory_has_any_binding(
    conn: &Connection,
    repo_id: &str,
    memory_id: &str,
) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM repo_memory_bindings WHERE repo_id = ?1 AND memory_id = ?2
         )",
        params![repo_id, memory_id],
        |row| row.get::<_, i64>(0),
    )? == 1)
}

/// Whether a `repo_memories` row with this id exists UNDER `repo_id`. The edge-drain source guard
/// requires this (not a bare existence check) for two reasons: node id is a GLOBAL PK, so a bare
/// check would let a peer edge (with a forged `owner_repo_id = repo_id`) reference a SIBLING repo's
/// colliding node id — attaching a this-repo edge to that repo's node, a boundary violation; and an
/// entirely ABSENT source (its node retro-condemned away) would abort the drain on the
/// `source_node_id` FK. Requiring the source to belong to THIS repo covers both.
fn node_in_repo(conn: &Connection, node_id: &str, repo_id: &str) -> anyhow::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM repo_memories WHERE id = ?1 AND repo_id = ?2)",
        params![node_id, repo_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|exists| exists != 0)
    .map_err(Into::into)
}

/// Fan the node's tag SET out to `repo_memory_tags` (whole-set replace) and refresh its FTS row —
/// the same two side tables the live create/update maintain, so a synced row reads back
/// identically.
fn write_node_children(tx: &Transaction<'_>, node_id: &str, tags: &[String]) -> anyhow::Result<()> {
    memory::replace_tags(tx, node_id, tags)?;
    memory::upsert_memory_fts(tx, node_id)?;
    Ok(())
}

/// Remove every `origin='synced'` node for this repo whose id is absent from the CURRENT projection
/// (a retro-condemn / revocation vacated it). The `origin='synced'` gate leaves a genuine local
/// ghost of the same id untouched. Returns the removal count.
///
/// Bindings and the contentless FTS shadow have no parent FK, so both are deleted explicitly before
/// the parent. The remaining tag / call-path / edge children still cascade.
fn remove_vanished_synced_nodes(
    tx: &Transaction<'_>,
    repo_id: &str,
    stream: StreamId,
) -> anyhow::Result<u32> {
    // The condemned set: this repo's synced rows absent from the current projection.
    const CONDEMNED: &str = "SELECT id FROM repo_memories
         WHERE repo_id = ?1 AND origin = 'synced'
           AND id NOT IN (SELECT node_id FROM content_projected_nodes WHERE stream_id = ?2)";
    // FTS first, while the parent rows still exist to name them (no FK to cascade this shadow).
    tx.execute(&format!("DELETE FROM repo_memory_fts WHERE memory_id IN ({CONDEMNED})"), params![
        repo_id,
        stream.to_bytes().as_slice()
    ])?;
    tx.execute(
        &format!(
            "DELETE FROM repo_memory_bindings WHERE repo_id = ?1 AND memory_id IN ({CONDEMNED})"
        ),
        params![repo_id, stream.to_bytes().as_slice()],
    )?;
    let removed = tx.execute(
        "DELETE FROM repo_memories
         WHERE repo_id = ?1 AND origin = 'synced'
           AND id NOT IN (SELECT node_id FROM content_projected_nodes WHERE stream_id = ?2)",
        params![repo_id, stream.to_bytes().as_slice()],
    )?;
    Ok(removed as u32)
}

/// Drop an existing `origin='synced'` mirror of `node_id` under `repo_id` (FTS shadow first — it
/// has no FK to cascade; bindings are also deleted explicitly, while the other children cascade).
/// Used
/// when a projected UPDATE to an already-materialized synced row fails the local validity gates:
/// the accepted value cannot be persisted, but the STALE prior value must not stay searchable
/// either (it is no longer the projection, so [`remove_vanished_synced_nodes`] — which only fires
/// when the node VANISHES from the projection — would never reach it). The `origin='synced'` +
/// `repo_id` gate leaves a genuine local row of the same id untouched: a peer's invalid edit never
/// destroys local content. Returns whether a row was removed.
fn remove_quarantined_synced_node(
    tx: &Transaction<'_>,
    repo_id: &str,
    node_id: &str,
) -> anyhow::Result<bool> {
    // FTS first, while the parent row still exists to name it (no FK cascades this shadow). The
    // subquery applies the same `origin='synced'` + repo gate as the row delete, so a local row's
    // FTS is never dropped.
    tx.execute(
        "DELETE FROM repo_memory_fts WHERE memory_id IN (
             SELECT id FROM repo_memories WHERE id = ?1 AND repo_id = ?2 AND origin = 'synced')",
        params![node_id, repo_id],
    )?;
    tx.execute(
        "DELETE FROM repo_memory_bindings WHERE repo_id = ?2 AND memory_id IN (
             SELECT id FROM repo_memories WHERE id = ?1 AND repo_id = ?2 AND origin = 'synced')",
        params![node_id, repo_id],
    )?;
    let removed = tx.execute(
        "DELETE FROM repo_memories WHERE id = ?1 AND repo_id = ?2 AND origin = 'synced'",
        params![node_id, repo_id],
    )?;
    Ok(removed > 0)
}

/// Whether a projected edge's DURABLE spec passes the SAME length caps the local `add_edge` write
/// path enforces (`target_anchor` / `target_repo_id` ≤ `MAX_EDGE_ANCHOR_LEN`). Peer content crosses
/// the wire only SHAPE-validated, so an older or compromised account device could author an
/// over-cap value that clears the envelope but bypasses this local boundary; such an edge must not
/// be persisted into `repo_node_edges`.
fn projected_edge_spec_is_valid(edge: &ProjectedContentEdge) -> anyhow::Result<()> {
    memory::validate_edge_len("target_anchor", &edge.spec.target_anchor)?;
    memory::validate_edge_len("target_repo_id", &edge.spec.target_repo_id)?;
    Ok(())
}

/// Drop an existing `origin='synced'` mirror of `edge_key` under `repo_id` (its children cascade
/// via FK; an edge has no FTS shadow). Symmetric to [`remove_quarantined_synced_node`]: used when a
/// projected edge fails the local length caps, so a now-invalid update never leaves a stale durable
/// spec searchable. The `origin='synced'` + `repo_id` gate spares a genuine local edge of the same
/// key. Returns whether a row was removed.
fn remove_quarantined_synced_edge(
    tx: &Transaction<'_>,
    repo_id: &str,
    edge_key: &str,
) -> anyhow::Result<bool> {
    let removed = tx.execute(
        "DELETE FROM repo_node_edges WHERE edge_key = ?1 AND repo_id = ?2 AND origin = 'synced'",
        params![edge_key, repo_id],
    )?;
    Ok(removed > 0)
}

/// One existing `repo_node_edges` row's DURABLE-spec columns — the only ones a converge compares or
/// updates. The content-addressed key fields (`source_node_id` / `relation` / `target_kind` /
/// `target_anchor`) are fixed by the `edge_key` and never change; the resolution triple
/// (`target_node_id` / `anchor_status`) is per-device and is deliberately NOT read here.
struct ExistingEdge {
    repo_id: String,
    target_repo_id: String,
}

fn read_existing_edge(conn: &Connection, edge_key: &str) -> anyhow::Result<Option<ExistingEdge>> {
    conn.query_row(
        "SELECT repo_id, target_repo_id FROM repo_node_edges WHERE edge_key = ?1",
        [edge_key],
        |row| Ok(ExistingEdge { repo_id: row.get(0)?, target_repo_id: row.get(1)? }),
    )
    .optional()
    .map_err(Into::into)
}

enum EdgeEffect {
    Written,
    Removed,
    Unchanged,
}

/// The `repo_node_edges` column values a projected edge maps to — the DURABLE spec only
/// (`owner_repo_id → repo_id`, `target_repo_id`, and the content-addressed key fields). The
/// resolution triple (`target_node_id` / `anchor_status`) is NEVER taken from the projection (see
/// [`EdgeColumns::from_projection`]): it is stored `unresolved` on INSERT and left untouched on a
/// converge, so the local read path owns resolution.
struct EdgeColumns {
    repo_id: String,
    source_node_id: String,
    relation: String,
    target_repo_id: String,
    target_kind: String,
    target_anchor: String,
    target_node_id: Option<String>,
    anchor_status: String,
}

impl EdgeColumns {
    fn from_projection(edge: &ProjectedContentEdge) -> Self {
        // The DURABLE spec ONLY — the projected `resolved` anchor is deliberately ignored. A
        // `ResolvedAnchor` is a PEER's per-device resolution (from a historical `Rebind`); splicing
        // its `target_repo_id` / `target_node_id` / `anchor_status` into this device's edge would
        // mint an inconsistent triple and overwrite local resolution with another device's view.
        // Store the target UNRESOLVED and let `reresolve_on_read` recompute the whole triple here.
        Self {
            repo_id: edge.spec.owner_repo_id.clone(),
            source_node_id: edge.spec.source_node_id.as_str().to_string(),
            relation: edge.spec.relation.as_db_str().to_string(),
            target_repo_id: edge.spec.target_repo_id.clone(),
            target_kind: edge.spec.target_kind.clone(),
            target_anchor: edge.spec.target_anchor.clone(),
            target_node_id: None,
            anchor_status: "unresolved".to_string(),
        }
    }

    /// Whether the existing row's DURABLE spec already equals the projection — comparing ONLY the
    /// owner + target repo (the key fields are identical by construction, the resolution triple is
    /// per-device and never converged). Comparing the resolution triple here would make a resolved
    /// local edge (`anchor_status='current'`) look "changed" against every projection (which
    /// carries no anchor) and re-converge to `unresolved` on every pass — churn, and a lost
    /// resolution.
    fn matches_durable(&self, existing: &ExistingEdge) -> bool {
        existing.repo_id == self.repo_id && existing.target_repo_id == self.target_repo_id
    }
}

/// Apply one projected edge to `repo_node_edges`, scoped to OUR repo. The owner is self-declared in
/// the (peer-signed) spec, so an edge in this repo's stream claiming a DIFFERENT `owner_repo_id` is
/// a malformed / hostile injection attempt and is skipped — never written into, nor removed from, a
/// sibling repo. A `present=0` tombstone REMOVES the edge (regardless of origin — a peer's remove
/// is the account-wide winner), scoped to this repo. A present edge converges: INSERT
/// `origin='synced'` if absent, else UPDATE the durable spec preserving origin and the per-device
/// resolution triple.
fn drain_edge(
    tx: &Transaction<'_>,
    repo_id: &str,
    edge: &ProjectedContentEdge,
    now_ms: i64,
) -> anyhow::Result<EdgeEffect> {
    // Repo boundary: only materialize/remove edges THIS repo owns. `owner_repo_id` is self-declared
    // in the signed spec; a foreign claim in our stream must not cross into a sibling repo.
    if edge.spec.owner_repo_id != repo_id {
        return Ok(EdgeEffect::Unchanged);
    }
    if !edge.present {
        // Tombstone: converge the removal for BOTH origins (a local edge a peer removed goes too),
        // scoped to this repo so a stray key can't delete a sibling's edge. The synced-only removal
        // is the retro-condemn anti-join, not this path.
        let removed = tx.execute(
            "DELETE FROM repo_node_edges WHERE edge_key = ?1 AND repo_id = ?2",
            params![edge.edge_key, repo_id],
        )?;
        return Ok(if removed > 0 { EdgeEffect::Removed } else { EdgeEffect::Unchanged });
    }
    // Untrusted-boundary length caps: apply the SAME limits `add_edge` enforces so a peer cannot
    // materialize an over-cap `target_anchor` / `target_repo_id`. An invalid edge is quarantined
    // (skipped + warned, never wedging the drain); a prior synced mirror of this key is removed so
    // a now-invalid update never leaves a stale durable spec behind.
    if let Err(err) = projected_edge_spec_is_valid(edge) {
        tracing::warn!(
            repo_id,
            edge_key = %edge.edge_key,
            error = %err,
            "quarantining an invalid synced edge: its durable spec violates a local length cap \
             (target_anchor/target_repo_id); skipped, not persisted",
        );
        let removed = remove_quarantined_synced_edge(tx, repo_id, &edge.edge_key)?;
        return Ok(if removed { EdgeEffect::Removed } else { EdgeEffect::Unchanged });
    }
    // Source guard: the source node must be a row THIS repo owns. The node and edge projection
    // registers are INDEPENDENT, so a retro-condemn can vacate the source while an accepted edge
    // still references it — its `source_node_id` FK would then abort the WHOLE drain (and every
    // subsequent open). And node id is a global PK, so a forged edge could name a SIBLING repo's
    // colliding id as its source; a bare existence check would attach a this-repo edge to that
    // repo's node. Requiring `repo_id` ownership covers both; skip the edge otherwise.
    if !node_in_repo(tx, edge.spec.source_node_id.as_str(), repo_id)? {
        return Ok(EdgeEffect::Unchanged);
    }
    let columns = EdgeColumns::from_projection(edge);
    match read_existing_edge(tx, &edge.edge_key)? {
        // `edge_key` is a global PK. A row already owned by ANOTHER repo must never be stolen /
        // rewritten into ours (symmetric to the node convergence guard). The source guard above
        // normally makes this unreachable — `edge_key` encodes the source, whose repo is the edge's
        // owner — but keep it explicit so a future invariant slip can't leak a converge across
        // repos.
        Some(existing) if existing.repo_id != repo_id => Ok(EdgeEffect::Unchanged),
        Some(existing) if columns.matches_durable(&existing) => Ok(EdgeEffect::Unchanged),
        Some(_) => {
            // Converge the durable spec (owner + target repo); `origin` and the per-device
            // resolution triple (`target_node_id` / `anchor_status`) are intentionally NOT in the
            // SET, so a locally-resolved edge keeps its resolution and its origin.
            tx.execute(
                "UPDATE repo_node_edges
                 SET repo_id = ?2, target_repo_id = ?3
                 WHERE edge_key = ?1",
                params![edge.edge_key, columns.repo_id, columns.target_repo_id],
            )?;
            Ok(EdgeEffect::Written)
        },
        None => {
            // First sight of this edge — received from a peer. The resolution triple is stored
            // `unresolved` (the projection carries none; the read path resolves it).
            tx.execute(
                "INSERT INTO repo_node_edges(
                     edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                     target_anchor, target_node_id, anchor_status, created_at_ms, origin)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'synced')",
                params![
                    edge.edge_key,
                    columns.repo_id,
                    columns.source_node_id,
                    columns.relation,
                    columns.target_repo_id,
                    columns.target_kind,
                    columns.target_anchor,
                    columns.target_node_id,
                    columns.anchor_status,
                    now_ms,
                ],
            )?;
            Ok(EdgeEffect::Written)
        },
    }
}

/// Remove every `origin='synced'` edge for this repo whose `edge_key` is absent from the projection
/// ENTIRELY — a retro-condemn vacated it. A `present=0` tombstone is still IN the projection, so it
/// is honored by `drain_edge`, not here. The origin gate leaves a local edge of the same key
/// intact.
fn remove_vanished_synced_edges(
    tx: &Transaction<'_>,
    repo_id: &str,
    stream: StreamId,
) -> anyhow::Result<u32> {
    let removed = tx.execute(
        "DELETE FROM repo_node_edges
         WHERE repo_id = ?1 AND origin = 'synced'
           AND edge_key NOT IN (SELECT edge_key FROM content_projected_edges WHERE stream_id = ?2)",
        params![repo_id, stream.to_bytes().as_slice()],
    )?;
    Ok(removed as u32)
}

#[cfg(test)]
mod tests {
    use rag_rat_query::memory::{
        self, RepoMemoryBindTarget, RepoMemoryCreate, edge_key, memory_by_id,
    };
    use rusqlite::Connection;

    use super::*;

    const REPO: &str = "repo-a";

    /// A DB with the memory schema, one registered repo, and the connection scoped to it.
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

    /// Create an unanchored `Concept` through the LIVE create path (mints the account + owner
    /// stream and projects the node) — the fixture the real-path tests build on.
    fn create_concept(conn: &Connection, title: &str) -> String {
        crate::memory_write::create_memory(conn, RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: title.to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: None,
            bind: RepoMemoryBindTarget::default(),
        })
        .unwrap()
        .memory
        .memory_id
    }

    /// Seed one row into `content_projected_nodes` with the exact `NodeContentRow` JSON shape the
    /// projector writes — the "a peer authored this and it folded accepted" fixture.
    #[allow(clippy::too_many_arguments)]
    fn seed_projected_node(
        conn: &Connection,
        stream: StreamId,
        node_id: &str,
        kind: &str,
        title: &str,
        body: &str,
        status: &str,
        tags: &[&str],
    ) {
        let content_json = serde_json::json!({
            "kind": kind,
            "title": title,
            "body": body,
            "confidence": "high",
            "source": "agent",
            "tags": tags,
            "payload": null,
        })
        .to_string();
        conn.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, ?2, ?3, ?4)",
            params![stream.to_bytes().as_slice(), node_id, content_json, status],
        )
        .unwrap();
    }

    /// Seed a projected node carrying an anchor snapshot. `anchors` is `(binding_kind, binding_id)`
    /// per row; `None` writes SQL NULL, the "nobody published bindings" state.
    fn seed_projected_node_with_anchors(
        conn: &Connection,
        stream: StreamId,
        node_id: &str,
        anchors: Option<&[(&str, &str)]>,
    ) {
        seed_projected_node(conn, stream, node_id, "Invariant", "t", "b", "active", &[]);
        let anchors_json = anchors.map(|anchors| {
            let rows: Vec<serde_json::Value> = anchors
                .iter()
                .map(|(kind, id)| {
                    serde_json::json!({
                        "binding_kind": kind,
                        "binding_id": id,
                        "path": "src/lib.rs",
                        "start_line": 1,
                        "end_line": 2,
                        "commit_hash": null,
                        "tracker": null,
                        "project": null,
                        "item_key": null,
                        "created_at_ms": 7,
                        "symbol_kind": null,
                        "signature_hash": null,
                        "moniker_tool": null,
                        "moniker_tool_version": null,
                    })
                })
                .collect();
            serde_json::to_string(&rows).unwrap()
        });
        conn.execute(
            "UPDATE content_projected_nodes SET anchors_json = ?3
             WHERE stream_id = ?1 AND node_id = ?2",
            params![stream.to_bytes().as_slice(), node_id, anchors_json],
        )
        .unwrap();
    }

    fn bindings_of(conn: &Connection, memory_id: &str) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT binding_kind, binding_id FROM repo_memory_bindings
                 WHERE repo_id = ?1 AND memory_id = ?2 ORDER BY binding_kind, binding_id",
            )
            .unwrap();
        let rows =
            stmt.query_map(params![REPO, memory_id], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        rows.map(Result::unwrap).collect()
    }

    /// The happy path: a synced memory arrives with no bindings here, so its author's snapshot
    /// seeds them — portable columns carried, every checkout-local column left at its default,
    /// which is the row state a `/5` apply produces.
    #[test]
    fn a_synced_memory_with_no_bindings_is_seeded_from_its_snapshot() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x44; 32]);
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_peer",
            Some(&[("symbol", "src/lib.rs::run"), ("path", "src/lib.rs")]),
        );

        drain_worker(&conn, stream, 1_000);

        assert_eq!(bindings_of(&conn, "mem_peer"), vec![
            ("path".to_string(), "src/lib.rs".to_string()),
            ("symbol".to_string(), "src/lib.rs::run".to_string()),
        ]);
        let (status, symbol_id): (String, Option<i64>) = conn
            .query_row(
                "SELECT anchor_status, symbol_id FROM repo_memory_bindings
                 WHERE memory_id = 'mem_peer' AND binding_kind = 'symbol'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "unverified", "local resolution state starts at its default");
        assert_eq!(symbol_id, None, "no checkout-local id is carried across the wire");
    }

    /// The gate, and the reason it is per-MEMORY rather than per-row: the validate/relocate loop
    /// re-keys `binding_id`, a PK column, so a per-row gate would find the pre-relocation identity
    /// absent and resurrect it beside the row the loop had moved.
    #[test]
    fn a_memory_that_already_holds_a_binding_is_never_seeded() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x44; 32]);
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_peer",
            Some(&[("symbol", "src/lib.rs::run")]),
        );
        drain_worker(&conn, stream, 1_000);

        // Stand in for the relocation loop: the row moves to a new identity.
        conn.execute(
            "UPDATE repo_memory_bindings SET binding_id = 'src/lib.rs::run_renamed'
             WHERE memory_id = 'mem_peer'",
            [],
        )
        .unwrap();

        // Forget the drain watermark so the next pass genuinely re-examines this node — otherwise
        // the drain short-circuits as caught-up and this test would pass with the gate removed.
        conn.execute("DELETE FROM oplog_meta WHERE key = 'content:drain-wm:' || hex(?1)", params![
            stream.to_bytes().as_slice()
        ])
        .unwrap();
        assert!(
            rag_rat_oplog::content_drain_needed(&conn, stream).unwrap(),
            "the second pass must actually run, or this test proves nothing",
        );

        // A later drain must not put the original identity back beside the relocated row.
        drain_worker(&conn, stream, 2_000);
        assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
            "symbol".to_string(),
            "src/lib.rs::run_renamed".to_string()
        )]);
    }

    /// An author publishing an EMPTY set is a statement that the memory has no bindings; it must
    /// seed nothing, and must not be confused with the `None` of nobody having published.
    #[test]
    fn neither_an_empty_snapshot_nor_an_absent_one_seeds_anything() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x44; 32]);
        seed_projected_node_with_anchors(&conn, stream, "mem_empty", Some(&[]));
        seed_projected_node_with_anchors(&conn, stream, "mem_absent", None);

        drain_worker(&conn, stream, 1_000);

        assert!(bindings_of(&conn, "mem_empty").is_empty());
        assert!(bindings_of(&conn, "mem_absent").is_empty());
    }

    /// A binding kind this store cannot produce is a newer peer's vocabulary: skipped row-wise,
    /// with the rest of the snapshot still seeded. `call_path` is skipped for a different
    /// reason — its supporting tables are in no replication scope, so it could never resolve
    /// here.
    #[test]
    fn an_unseedable_kind_is_skipped_without_losing_the_rest() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x44; 32]);
        seed_projected_node_with_anchors(
            &conn,
            stream,
            "mem_peer",
            Some(&[
                ("symbol", "src/lib.rs::run"),
                ("call_path", "abc123"),
                ("from_the_future", "whatever"),
            ]),
        );

        drain_worker(&conn, stream, 1_000);

        assert_eq!(bindings_of(&conn, "mem_peer"), vec![(
            "symbol".to_string(),
            "src/lib.rs::run".to_string()
        )]);
    }

    /// Seed one row into `content_projected_edges`. `resolved` is `(target_repo, target_node,
    /// anchor_status)` when the projection carries a resolved anchor.
    #[allow(clippy::too_many_arguments)]
    fn seed_projected_edge(
        conn: &Connection,
        stream: StreamId,
        edge_key: &str,
        source: &str,
        relation: &str,
        target_kind: &str,
        target_anchor: &str,
        present: bool,
    ) {
        let spec_json = serde_json::json!({
            "source_node_id": source,
            "relation": relation,
            "target_repo_id": REPO,
            "target_kind": target_kind,
            "target_anchor": target_anchor,
            "owner_repo_id": REPO,
        })
        .to_string();
        conn.execute(
            "INSERT INTO content_projected_edges(
                 stream_id, edge_key, spec_json, resolved_json, present)
             VALUES (?1, ?2, ?3, NULL, ?4)",
            params![stream.to_bytes().as_slice(), edge_key, spec_json, present as i64],
        )
        .unwrap();
    }

    /// Insert a locally-authored `repo_memories` row directly (origin defaults to `local`).
    fn insert_local_memory(conn: &Connection, id: &str, title: &str, body: &str, status: &str) {
        insert_local_memory_in_repo(conn, id, title, body, status, REPO);
    }

    /// As [`insert_local_memory`] but stamped with an explicit `repo_id` — used to plant a SIBLING
    /// repo's row for the cross-repo boundary tests.
    fn insert_local_memory_in_repo(
        conn: &Connection,
        id: &str,
        title: &str,
        body: &str,
        status: &str,
        repo: &str,
    ) {
        conn.execute(
            "INSERT INTO repo_memories(
                 id, kind, title, body, confidence, status, created_by, created_at_ms,
                 updated_at_ms, source, input_hash, memory_version, repo_id)
             VALUES (?1, 'Invariant', ?2, ?3, 'high', ?4, 'agent', 100, 100, 'agent', 'h', 'v1',
                 ?5)",
            params![id, title, body, status, repo],
        )
        .unwrap();
    }

    /// Insert a locally-authored edge directly (origin defaults to `local`); returns its key.
    fn insert_local_edge(conn: &Connection, source: &str, relation: &str, target: &str) -> String {
        let key = edge_key(source, relation, "node", target);
        conn.execute(
            "INSERT INTO repo_node_edges(
                 edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                 target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?2, 'node', ?5, ?5, 'current', 100)",
            params![key, REPO, source, relation, target],
        )
        .unwrap();
        key
    }

    /// Run the in-tx drain worker over `stream` and return its outcome.
    fn drain_worker(conn: &Connection, stream: StreamId, now_ms: i64) -> DrainOutcome {
        let tx = conn.unchecked_transaction().unwrap();
        let outcome = drain_synced_stream_in_tx(&tx, REPO, stream, now_ms).unwrap();
        tx.commit().unwrap();
        outcome
    }

    fn origin_of(conn: &Connection, id: &str) -> String {
        conn.query_row("SELECT origin FROM repo_memories WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    fn status_of(conn: &Connection, id: &str) -> String {
        conn.query_row("SELECT status FROM repo_memories WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    fn updated_at_of(conn: &Connection, id: &str) -> i64 {
        conn.query_row("SELECT updated_at_ms FROM repo_memories WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    fn edge_exists(conn: &Connection, key: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM repo_node_edges WHERE edge_key = ?1)",
            [key],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            != 0
    }

    fn origin_of_edge(conn: &Connection, key: &str) -> String {
        conn.query_row("SELECT origin FROM repo_node_edges WHERE edge_key = ?1", [key], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn fts_row_exists(conn: &Connection, id: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM repo_memory_fts WHERE memory_id = ?1)",
            [id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            != 0
    }

    fn content_entry_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM content_entries", [], |r| r.get(0)).unwrap()
    }

    // --- Task 1: repo attribution by forward derivation (repo-identity-skew pin) ---

    /// The drain derives the owner stream FORWARD from `(repo_id, account_id)` and must land on the
    /// EXACT stream the authoring path projected into — the guard against repo-identity skew. The
    /// derivation is a pure function of the account + repo (no device/checkout input), so it is
    /// stable across calls.
    #[test]
    fn the_drain_derives_the_same_owner_stream_authoring_projected_into() {
        let conn = scoped_conn();
        let id = create_concept(&conn, "seed");
        let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
        let projected: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM content_projected_nodes WHERE stream_id = ?1 AND node_id = \
                 ?2",
                params![stream.to_bytes().as_slice(), id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(projected, 1, "the authored node is projected under the forward-derived stream");
        assert_eq!(
            rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap(),
            stream,
            "the derivation is stable (a pure function of account + repo)",
        );
    }

    // --- Task 2: node drain, happy path ---

    #[test]
    fn a_projected_node_materializes_as_a_synced_memory_with_tags() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_x", "Invariant", "title x", "body x", "active", &[
            "beta", "alpha",
        ]);

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(outcome.nodes_written, 1);

        let memory = memory_by_id(&conn, "mem_x").unwrap().expect("materialized as a real row");
        assert_eq!(memory.title, "title x");
        assert_eq!(memory.body, "body x");
        assert_eq!(memory.tags, vec!["alpha".to_string(), "beta".to_string()], "tags fanned out");
        assert_eq!(origin_of(&conn, "mem_x"), "synced", "the drain writes an origin='synced' row");
        let repo: String = conn
            .query_row("SELECT repo_id FROM repo_memories WHERE id = 'mem_x'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(repo, REPO, "the synced row is stamped with the drained repo");
    }

    // --- Task 3: converge a projected local row; leave an UNPROJECTED local row untouched ---

    /// A local row that ANOTHER device changed appears in the projection (the account-wide LWW
    /// winner) with the new value: the drain CONVERGES the local row to it, preserving
    /// `origin='local'` (it is NOT frozen). A local row with NO projection entry is a pending,
    /// not-yet-reconciled edit and is left untouched.
    #[test]
    fn a_projected_local_row_converges_and_an_unprojected_local_row_is_untouched() {
        let conn = scoped_conn();
        // A local row the peer updated + obsoleted: the projection holds the winning value.
        insert_local_memory(&conn, "mem_shared", "OLD title", "old body", "active");
        memory::replace_tags(&conn, "mem_shared", &["oldtag".to_string()]).unwrap();
        // A local row with NO projection entry: a pending local edit not yet reconciled.
        insert_local_memory(&conn, "mem_pending", "pending title", "pending body", "active");
        memory::replace_tags(&conn, "mem_pending", &["pendingtag".to_string()]).unwrap();

        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(
            &conn,
            stream,
            "mem_shared",
            "Decision",
            "NEW title",
            "new body",
            "obsolete",
            &["newtag"],
        );

        let outcome = drain_worker(&conn, stream, 2_000);
        assert_eq!(outcome.nodes_written, 1, "the projected local row converges");
        assert_eq!(outcome.nodes_removed, 0, "the unprojected local row is not removed");

        // Converged to the projection value, `origin` preserved as local.
        let (kind, title, body, status, origin): (String, String, String, String, String) = conn
            .query_row(
                "SELECT kind, title, body, status, origin FROM repo_memories WHERE id = \
                 'mem_shared'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            (kind.as_str(), title.as_str(), body.as_str(), status.as_str(), origin.as_str()),
            ("Decision", "NEW title", "new body", "obsolete", "local"),
            "a projected local row converges to the account-wide value, origin preserved",
        );
        assert_eq!(memory::tags_for_memory(&conn, "mem_shared").unwrap(), vec![
            "newtag".to_string()
        ]);

        // The unprojected local row is byte-for-byte untouched.
        let (title2, origin2): (String, String) = conn
            .query_row(
                "SELECT title, origin FROM repo_memories WHERE id = 'mem_pending'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (title2.as_str(), origin2.as_str()),
            ("pending title", "local"),
            "a local row absent from the projection is a pending edit, left untouched",
        );
        assert_eq!(memory::tags_for_memory(&conn, "mem_pending").unwrap(), vec![
            "pendingtag".to_string()
        ],);
    }

    /// A memory created on device A, then updated (content) and — separately — obsoleted (status)
    /// by device B: A's drain converges its local row to B's value each time, and `origin`
    /// stays `'local'` throughout (A remains the author; convergence is not an ownership
    /// change).
    #[test]
    fn a_remote_update_then_obsolete_converges_the_local_row_preserving_origin() {
        let conn = scoped_conn();
        insert_local_memory(&conn, "mem_a", "v1 title", "v1 body", "active");
        let stream = StreamId::from_bytes([0x44; 32]);

        // Device B updated the content.
        seed_projected_node(
            &conn,
            stream,
            "mem_a",
            "Invariant",
            "v2 title",
            "v2 body",
            "active",
            &[],
        );
        let out = drain_worker(&conn, stream, 1_000);
        assert_eq!(out.nodes_written, 1);
        assert_eq!(
            memory_by_id(&conn, "mem_a").unwrap().unwrap().body,
            "v2 body",
            "A converges to B's content",
        );
        assert_eq!(origin_of(&conn, "mem_a"), "local", "convergence preserves A's authorship");

        // Device B then obsoleted it.
        conn.execute(
            "UPDATE content_projected_nodes SET status = 'obsolete' WHERE node_id = 'mem_a'",
            [],
        )
        .unwrap();
        let out = drain_worker(&conn, stream, 2_000);
        assert_eq!(out.nodes_written, 1);
        assert_eq!(status_of(&conn, "mem_a"), "obsolete", "A converges to B's status");
        assert_eq!(origin_of(&conn, "mem_a"), "local", "still A's row, still local");
    }

    // --- Task 4: status flip is an UPDATE, never a DELETE ---

    #[test]
    fn a_projected_obsolete_status_updates_the_synced_row_and_does_not_delete_it() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_x", "Invariant", "t", "b", "active", &[]);
        drain_worker(&conn, stream, 1_000);
        assert_eq!(status_of(&conn, "mem_x"), "active");

        conn.execute(
            "UPDATE content_projected_nodes SET status = 'obsolete' WHERE node_id = 'mem_x'",
            [],
        )
        .unwrap();
        let outcome = drain_worker(&conn, stream, 2_000);
        assert_eq!(outcome.nodes_written, 1, "the status change is a write");
        assert_eq!(outcome.nodes_removed, 0, "an obsolete status flip is never a delete");
        assert_eq!(status_of(&conn, "mem_x"), "obsolete");
        assert!(memory_by_id(&conn, "mem_x").unwrap().is_some(), "the row still exists");
    }

    // --- Task 5: edge drain + FK order ---

    #[test]
    fn a_present_edge_upserts_a_synced_edge_and_a_tombstone_removes_it() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
        seed_projected_node(&conn, stream, "mem_b", "Invariant", "b", "b", "active", &[]);
        let key = edge_key("mem_a", "relates_to", "node", "mem_b");
        seed_projected_edge(&conn, stream, &key, "mem_a", "relates_to", "node", "mem_b", true);

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(outcome.nodes_written, 2, "both source and target nodes materialize first");
        assert_eq!(outcome.edges_written, 1);
        let (origin, source): (String, String) = conn
            .query_row(
                "SELECT origin, source_node_id FROM repo_node_edges WHERE edge_key = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((origin.as_str(), source.as_str()), ("synced", "mem_a"));

        conn.execute("UPDATE content_projected_edges SET present = 0 WHERE edge_key = ?1", [&key])
            .unwrap();
        let outcome = drain_worker(&conn, stream, 2_000);
        assert_eq!(outcome.edges_removed, 1, "the tombstone removes the synced edge");
        assert!(!edge_exists(&conn, &key));
    }

    /// A local edge that ANOTHER device removed appears in the projection as a `present=0`
    /// tombstone; the drain must remove it here too — a peer's remove is the account-wide
    /// winner, so the tombstone path is NOT origin-gated (the P1 convergence bug: freezing it
    /// left the edge forever).
    #[test]
    fn a_tombstone_removes_a_local_edge_of_the_same_key() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        insert_local_memory(&conn, "mem_a", "a", "b", "active");
        let key = insert_local_edge(&conn, "mem_a", "relates_to", "mem_b");
        assert_eq!(origin_of_edge(&conn, &key), "local");
        // The projection carries a present=0 tombstone for the SAME key.
        seed_projected_edge(&conn, stream, &key, "mem_a", "relates_to", "node", "mem_b", false);

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(outcome.edges_removed, 1, "a peer's remove converges regardless of origin");
        assert!(!edge_exists(&conn, &key), "the local edge a peer removed is removed here too");
    }

    /// A local edge whose durable spec (`target_repo_id`) another device changed converges to the
    /// projection value, preserving `origin='local'` AND the per-device resolution triple
    /// (`target_node_id` / `anchor_status`) — the read path owns resolution, the drain must not
    /// wipe it.
    #[test]
    fn a_remote_spec_change_converges_a_local_edge_preserving_origin_and_resolution() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        insert_local_memory(&conn, "mem_a", "a", "b", "active");
        // A local, resolved edge whose stored target repo is stale (add-time snapshot).
        let key = edge_key("mem_a", "relates_to", "node", "mem_b");
        conn.execute(
            "INSERT INTO repo_node_edges(
                 edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                 target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, ?2, 'mem_a', 'relates_to', 'stale-target-repo', 'node', 'mem_b', 'mem_b',
                 'current', 100)",
            params![key, REPO],
        )
        .unwrap();
        // The projection carries the peer's current target repo for the same key.
        conn.execute(
            "INSERT INTO content_projected_edges(stream_id, edge_key, spec_json, resolved_json, \
             present)
             VALUES (?1, ?2, ?3, NULL, 1)",
            params![
                stream.to_bytes().as_slice(),
                key,
                serde_json::json!({
                    "source_node_id": "mem_a",
                    "relation": "relates_to",
                    "target_repo_id": "current-target-repo",
                    "target_kind": "node",
                    "target_anchor": "mem_b",
                    "owner_repo_id": REPO,
                })
                .to_string(),
            ],
        )
        .unwrap();

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(outcome.edges_written, 1, "the durable spec converges");
        let (repo_id, target_repo, target_node, anchor, origin): (
            String,
            String,
            Option<String>,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT repo_id, target_repo_id, target_node_id, anchor_status, origin
                 FROM repo_node_edges WHERE edge_key = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            target_repo, "current-target-repo",
            "target_repo_id converges to the peer value"
        );
        assert_eq!(repo_id, REPO, "owner repo unchanged");
        assert_eq!(origin, "local", "convergence preserves the local origin");
        assert_eq!(
            (target_node.as_deref(), anchor.as_str()),
            (Some("mem_b"), "current"),
            "the per-device resolution triple is preserved, not wiped to unresolved",
        );

        // Idempotent: a re-drain now matches the durable spec and writes nothing.
        let again = drain_worker(&conn, stream, 2_000);
        assert_eq!(again, DrainOutcome::default(), "converged edge is a no-op on re-drain");
    }

    // --- Task 6: retro-condemn removal ---

    #[test]
    fn a_condemned_synced_node_is_removed_on_re_drain_and_a_local_row_survives() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_synced", "Invariant", "s", "b", "active", &[]);
        drain_worker(&conn, stream, 1_000);
        assert!(memory_by_id(&conn, "mem_synced").unwrap().is_some());

        // A local row absent from the projection is a genuine local ghost, NOT a condemned synced
        // row — the origin gate must spare it.
        insert_local_memory(&conn, "mem_local", "l", "b", "active");
        // Retro-condemn: the synced node's projection row vanishes entirely.
        conn.execute("DELETE FROM content_projected_nodes WHERE node_id = 'mem_synced'", [])
            .unwrap();

        assert!(fts_row_exists(&conn, "mem_synced"), "the materialized synced row has an FTS row");

        let outcome = drain_worker(&conn, stream, 2_000);
        assert_eq!(outcome.nodes_removed, 1);
        assert!(
            memory_by_id(&conn, "mem_synced").unwrap().is_none(),
            "the condemned synced row is removed",
        );
        assert!(
            !fts_row_exists(&conn, "mem_synced"),
            "the contentless FTS shadow (no FK) is cleaned up in the same txn, not orphaned",
        );
        assert!(
            memory_by_id(&conn, "mem_local").unwrap().is_some(),
            "a local row of an absent id survives the origin gate",
        );
    }

    /// A projected edge carrying a PEER's `Rebind` resolution must NOT import it: the durable
    /// target repo comes from the signed spec, and the per-device resolution triple is stored
    /// `unresolved` for the local read path to recompute (not spliced from another device's
    /// view).
    #[test]
    fn a_projected_rebind_anchor_is_ignored_when_materializing_an_edge() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
        seed_projected_node(&conn, stream, "mem_b", "Invariant", "b", "b", "active", &[]);
        let key = edge_key("mem_a", "relates_to", "node", "mem_b");
        conn.execute(
            "INSERT INTO content_projected_edges(stream_id, edge_key, spec_json, resolved_json, \
             present)
             VALUES (?1, ?2, ?3, ?4, 1)",
            params![
                stream.to_bytes().as_slice(),
                key,
                serde_json::json!({
                    "source_node_id": "mem_a",
                    "relation": "relates_to",
                    "target_repo_id": "spec-repo",
                    "target_kind": "node",
                    "target_anchor": "mem_b",
                    "owner_repo_id": REPO,
                })
                .to_string(),
                serde_json::json!({
                    "target_repo_id": "peer-resolved-repo",
                    "target_node_id": "peer-node",
                    "anchor_status": "gone",
                })
                .to_string(),
            ],
        )
        .unwrap();

        drain_worker(&conn, stream, 1_000);
        let (target_repo, target_node, anchor): (String, Option<String>, String) = conn
            .query_row(
                "SELECT target_repo_id, target_node_id, anchor_status FROM repo_node_edges
                 WHERE edge_key = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            target_repo, "spec-repo",
            "the durable signed target repo is materialized, not the peer's Rebind resolution",
        );
        assert_eq!(
            (target_node.as_deref(), anchor.as_str()),
            (None, "unresolved"),
            "the peer's per-device resolution triple is not imported",
        );
    }

    #[test]
    fn a_condemned_synced_edge_is_removed_on_re_drain() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
        seed_projected_node(&conn, stream, "mem_b", "Invariant", "b", "b", "active", &[]);
        let key = edge_key("mem_a", "relates_to", "node", "mem_b");
        seed_projected_edge(&conn, stream, &key, "mem_a", "relates_to", "node", "mem_b", true);
        drain_worker(&conn, stream, 1_000);
        assert!(edge_exists(&conn, &key));

        // The whole edge row (not a present=0 tombstone) vanishes from the projection.
        conn.execute("DELETE FROM content_projected_edges WHERE edge_key = ?1", [&key]).unwrap();
        let outcome = drain_worker(&conn, stream, 2_000);
        assert_eq!(outcome.edges_removed, 1, "a vanished synced edge is removed on re-drain");
        assert!(!edge_exists(&conn, &key));
    }

    // --- Hardening: robustness + cross-repo boundary against malformed peer content ---

    /// The node and edge projection registers are independent, so an accepted edge can legitimately
    /// reference a source node that was retro-condemned away. Its FK would abort the whole drain
    /// (and every open) — the drain must SKIP it and still materialize the rest.
    #[test]
    fn a_projected_edge_with_a_missing_source_node_is_skipped_not_fatal() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        // A valid node + edge, plus a dangling edge whose source node is not in the projection.
        seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
        seed_projected_node(&conn, stream, "mem_b", "Invariant", "b", "b", "active", &[]);
        let good = edge_key("mem_a", "relates_to", "node", "mem_b");
        seed_projected_edge(&conn, stream, &good, "mem_a", "relates_to", "node", "mem_b", true);
        let dangling = edge_key("ghost", "relates_to", "node", "mem_b");
        seed_projected_edge(&conn, stream, &dangling, "ghost", "relates_to", "node", "mem_b", true);

        // The drain succeeds (no FK abort), materializes the good edge, and skips the dangling one.
        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(outcome.nodes_written, 2);
        assert_eq!(outcome.edges_written, 1, "only the edge with a materialized source is written");
        assert!(edge_exists(&conn, &good));
        assert!(!edge_exists(&conn, &dangling), "the dangling edge is skipped, not fatal");
    }

    /// Node id is a global PK. A peer stream naming an id another repo already owns must NOT
    /// overwrite that sibling's row.
    #[test]
    fn a_projected_node_owned_by_a_sibling_repo_is_not_overwritten() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        // A sibling repo already owns `mem_shared`.
        insert_local_memory_in_repo(
            &conn,
            "mem_shared",
            "SIBLING title",
            "sibling body",
            "active",
            "repo-b",
        );
        // Our stream projects a node with the same id, different content.
        seed_projected_node(
            &conn,
            stream,
            "mem_shared",
            "Decision",
            "HOSTILE title",
            "hostile body",
            "obsolete",
            &[],
        );

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(outcome.nodes_written, 0, "a sibling repo's row is never converged");
        let (title, repo_id): (String, String) = conn
            .query_row(
                "SELECT title, repo_id FROM repo_memories WHERE id = 'mem_shared'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (title.as_str(), repo_id.as_str()),
            ("SIBLING title", "repo-b"),
            "the sibling row is byte-for-byte unchanged",
        );
    }

    /// An edge's `owner_repo_id` is self-declared in the peer-signed spec. An edge in OUR stream
    /// claiming a foreign owner must not be injected into that sibling repo.
    #[test]
    fn a_projected_edge_claiming_a_foreign_owner_repo_is_not_injected() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
        let key = edge_key("mem_a", "relates_to", "node", "mem_b");
        // A hostile edge in our stream claiming owner_repo_id = a sibling repo.
        conn.execute(
            "INSERT INTO content_projected_edges(stream_id, edge_key, spec_json, resolved_json, \
             present)
             VALUES (?1, ?2, ?3, NULL, 1)",
            params![
                stream.to_bytes().as_slice(),
                key,
                serde_json::json!({
                    "source_node_id": "mem_a",
                    "relation": "relates_to",
                    "target_repo_id": "repo-b",
                    "target_kind": "node",
                    "target_anchor": "mem_b",
                    "owner_repo_id": "repo-b",
                })
                .to_string(),
            ],
        )
        .unwrap();

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(outcome.edges_written, 0, "an edge claiming a foreign owner is not written");
        assert!(!edge_exists(&conn, &key), "nothing is injected into the sibling repo");
    }

    /// Even with a truthful `owner_repo_id = repo_id`, an edge whose SOURCE node id collides with a
    /// sibling repo's node must not materialize — it would attach a this-repo edge to the sibling's
    /// node (node id is a global PK). The source guard requires the source to belong to this repo.
    #[test]
    fn a_projected_edge_whose_source_belongs_to_a_sibling_repo_is_skipped() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        // A node id owned by a SIBLING repo (not ours).
        insert_local_memory_in_repo(&conn, "mem_sibling", "sib", "b", "active", "repo-b");
        // Our stream projects an edge (owner = us) whose source is that sibling id.
        let key = edge_key("mem_sibling", "relates_to", "node", "mem_b");
        seed_projected_edge(
            &conn,
            stream,
            &key,
            "mem_sibling",
            "relates_to",
            "node",
            "mem_b",
            true,
        );

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(
            outcome.edges_written, 0,
            "an edge whose source is a sibling repo's node is not materialized",
        );
        assert!(!edge_exists(&conn, &key));
    }

    /// The store-global drain runs at open BEFORE the connection scope is installed, so on a
    /// multi-repo store the active-repo scope is unresolvable. The synced row's FTS shadow must
    /// still carry `repo_id` (copied from the row the drain stamped) — otherwise
    /// `memory_search`'s repo filter never matches it and the primary "searchable now" behavior
    /// fails until some later scoped write repairs it.
    #[test]
    fn a_drained_synced_node_is_repo_stamped_in_fts_even_when_scope_is_unresolvable() {
        let conn = scoped_conn();
        // Reproduce the open-time, multi-repo condition: a second registered repo (so
        // `sole_repo_id` can't pick one) and no `connection_context` scope.
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES \
             ('repo-b','repo-b',0)",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM temp.connection_context WHERE key = 'repo_id'", []).unwrap();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_x", "Invariant", "findable", "b", "active", &[]);

        drain_worker(&conn, stream, 1_000);

        let fts_repo: Option<String> = conn
            .query_row("SELECT repo_id FROM repo_memory_fts WHERE memory_id = 'mem_x'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            fts_repo.as_deref(),
            Some(REPO),
            "the drained row's FTS carries repo_id even when the connection scope is unresolvable",
        );
    }

    /// Peer content is only wire-shape-validated, so an older / compromised device could project a
    /// node that violates a local content rule (here: an unknown `kind`). It must be QUARANTINED
    /// (skipped, not persisted) without wedging the drain — the valid siblings still materialize.
    #[test]
    fn an_invalid_synced_node_is_quarantined_and_does_not_wedge_the_drain() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_ok", "Invariant", "ok", "b", "active", &[]);
        // An unknown kind — rejected by `validate_kind`, which the create path enforces.
        seed_projected_node(&conn, stream, "mem_bad", "NotAValidKind", "bad", "b", "active", &[]);
        // An oversized body — beyond `MAX_MEMORY_BODY_LEN`, which the create path caps.
        seed_projected_node(
            &conn,
            stream,
            "mem_big",
            "Invariant",
            "big",
            &"x".repeat(1_000_000),
            "active",
            &[],
        );

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(outcome.nodes_written, 1, "only the valid node is materialized");
        assert!(memory_by_id(&conn, "mem_ok").unwrap().is_some());
        assert!(
            memory_by_id(&conn, "mem_bad").unwrap().is_none(),
            "the unknown-kind node is quarantined, not persisted",
        );
        assert!(
            memory_by_id(&conn, "mem_big").unwrap().is_none(),
            "the oversized-body node is quarantined, not persisted",
        );
    }

    /// A peer edits an already-materialized synced memory to content that fails LOCAL validation.
    /// The accepted value cannot be persisted, but the STALE prior synced row (still in the
    /// projection, so retro-condemn never reaches it) must be removed — not left searchable
    /// forever.
    #[test]
    fn a_projected_update_to_invalid_content_removes_the_stale_synced_row() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        // First drain: the node is valid and materializes as a searchable synced row.
        seed_projected_node(&conn, stream, "mem_x", "Invariant", "ok", "b", "active", &[]);
        drain_worker(&conn, stream, 1_000);
        assert!(memory_by_id(&conn, "mem_x").unwrap().is_some());
        assert!(fts_row_exists(&conn, "mem_x"), "the materialized synced row has an FTS row");

        // A peer's accepted edit changes the projected content to an unknown kind (invalid
        // locally).
        conn.execute(
            "UPDATE content_projected_nodes
             SET content_json = json_set(content_json, '$.kind', 'NotAValidKind')
             WHERE node_id = 'mem_x'",
            [],
        )
        .unwrap();

        let outcome = drain_worker(&conn, stream, 2_000);
        assert_eq!(
            outcome.nodes_removed, 1,
            "the stale synced mirror is removed, counted as removed"
        );
        assert_eq!(outcome.nodes_written, 0, "the invalid value is never persisted");
        assert!(
            memory_by_id(&conn, "mem_x").unwrap().is_none(),
            "the stale prior synced row is not left searchable under an invalidated projection",
        );
        assert!(
            !fts_row_exists(&conn, "mem_x"),
            "the contentless FTS shadow is cleaned up in the same txn, not orphaned",
        );

        // Idempotent: a re-drain still sees the invalid projection but has nothing left to remove.
        let again = drain_worker(&conn, stream, 3_000);
        assert_eq!(again, DrainOutcome::default(), "no row to remove twice — a no-op re-drain");
    }

    /// The quarantine-removal is gated to `origin='synced'`: a peer's invalid edit must never
    /// destroy a genuine local row of the same id (the user's own authored content).
    #[test]
    fn a_projected_update_to_invalid_content_spares_a_local_row() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        // A locally-authored row (origin='local'), and a projection that would converge it but is
        // invalid.
        insert_local_memory(&conn, "mem_local", "mine", "b", "active");
        conn.execute(
            "INSERT INTO repo_memory_bindings(
                 repo_id, memory_id, binding_kind, binding_id, path, anchor_status, created_at_ms)
             VALUES (?1, 'mem_local', 'path', 'src/lib.rs', 'src/lib.rs', 'current', 0)",
            [REPO],
        )
        .unwrap();
        seed_projected_node(&conn, stream, "mem_local", "NotAValidKind", "peer", "b", "active", &[
        ]);

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(
            outcome.nodes_removed, 0,
            "a local row is never removed by an invalid peer edit"
        );
        assert!(
            memory_by_id(&conn, "mem_local").unwrap().is_some(),
            "the user's local content survives an invalid projected edit",
        );
        let binding_exists: bool = conn
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM repo_memory_bindings
                     WHERE repo_id = ?1 AND memory_id = 'mem_local')",
                [REPO],
                |row| row.get(0),
            )
            .unwrap();
        assert!(binding_exists, "the local memory's anchors survive with its content");
    }

    /// A projected node whose tag exceeds the local 64-byte cap must be quarantined here, not left
    /// to error inside `write_node_children`/`replace_tags` and roll back (and wedge) the whole
    /// drain.
    #[test]
    fn an_oversized_tag_on_a_synced_node_is_quarantined_not_fatal() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_ok", "Invariant", "ok", "b", "active", &["fine"]);
        let big_tag = "x".repeat(65);
        seed_projected_node(&conn, stream, "mem_tag", "Invariant", "t", "b", "active", &[&big_tag]);

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(outcome.nodes_written, 1, "only the valid node materializes");
        assert!(memory_by_id(&conn, "mem_ok").unwrap().is_some());
        assert!(
            memory_by_id(&conn, "mem_tag").unwrap().is_none(),
            "an oversized tag quarantines the node rather than wedging the whole drain",
        );
    }

    /// A projected edge whose `target_anchor` exceeds `MAX_EDGE_ANCHOR_LEN` must be quarantined at
    /// the untrusted boundary — the same cap the local `add_edge` write path enforces.
    #[test]
    fn an_oversized_edge_anchor_on_a_synced_edge_is_quarantined_not_persisted() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_a", "Invariant", "a", "b", "active", &[]);
        let big_anchor = "x".repeat(memory::MAX_EDGE_ANCHOR_LEN + 1);
        let key = edge_key("mem_a", "relates_to", "node", &big_anchor);
        seed_projected_edge(&conn, stream, &key, "mem_a", "relates_to", "node", &big_anchor, true);

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(outcome.edges_written, 0, "the over-cap edge is not persisted");
        assert!(
            !edge_exists(&conn, &key),
            "an oversized edge anchor is quarantined at the boundary"
        );
    }

    /// An existing edge row owned by a SIBLING repo (a global-PK `edge_key` collision) must not be
    /// stolen or rewritten into this repo by a converge — symmetric to the node guard.
    #[test]
    fn a_converge_never_steals_a_sibling_repos_edge_row() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        insert_local_memory(&conn, "mem_a", "a", "b", "active");
        let key = edge_key("mem_a", "relates_to", "node", "mem_b");
        // An existing edge row on this key owned by a sibling repo (the collision codex describes).
        conn.execute(
            "INSERT INTO repo_node_edges(
                 edge_key, repo_id, source_node_id, relation, target_repo_id, target_kind,
                 target_anchor, target_node_id, anchor_status, created_at_ms)
             VALUES (?1, 'repo-b', 'mem_a', 'relates_to', 'repo-b', 'node', 'mem_b', 'mem_b',
                 'current', 100)",
            [&key],
        )
        .unwrap();
        // Our stream projects the same key (owner = us, different target repo).
        seed_projected_edge(&conn, stream, &key, "mem_a", "relates_to", "node", "mem_b", true);

        let outcome = drain_worker(&conn, stream, 1_000);
        assert_eq!(outcome.edges_written, 0, "the sibling's edge is not converged");
        let (repo_id, target_repo): (String, String) = conn
            .query_row(
                "SELECT repo_id, target_repo_id FROM repo_node_edges WHERE edge_key = ?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (repo_id.as_str(), target_repo.as_str()),
            ("repo-b", "repo-b"),
            "the sibling repo's edge is left byte-for-byte unchanged, not stolen",
        );
    }

    // --- Task 7: idempotence ---

    #[test]
    fn re_running_the_drain_over_an_unchanged_projection_is_a_no_op() {
        let conn = scoped_conn();
        let stream = StreamId::from_bytes([0x33; 32]);
        seed_projected_node(&conn, stream, "mem_x", "Invariant", "t", "b", "active", &["a", "b"]);
        seed_projected_node(&conn, stream, "mem_y", "Invariant", "y", "b", "active", &[]);
        let key = edge_key("mem_x", "relates_to", "node", "mem_y");
        seed_projected_edge(&conn, stream, &key, "mem_x", "relates_to", "node", "mem_y", true);

        let first = drain_worker(&conn, stream, 1_000);
        assert_eq!(first.nodes_written, 2);
        assert_eq!(first.edges_written, 1);
        let updated_before = updated_at_of(&conn, "mem_x");

        let second = drain_worker(&conn, stream, 9_999);
        assert_eq!(
            second,
            DrainOutcome::default(),
            "a re-drain over an unchanged projection writes and removes nothing",
        );
        assert_eq!(
            updated_at_of(&conn, "mem_x"),
            updated_before,
            "updated_at_ms is not bumped on a no-op re-drain",
        );
    }

    // --- Task 8: no echo — the round-trip is proven end-to-end ---

    /// A drained `origin='synced'` row must NOT be re-authored back into the signed `/3` log by the
    /// next reconcile — the authoring-side `origin='local'` gate plus the projection anti-join make
    /// the round-trip non-echoing. Exercised on the REAL path (real owner stream + public entry).
    #[test]
    fn a_drained_synced_row_is_not_re_authored_by_the_next_reconcile() {
        let conn = scoped_conn();
        create_concept(&conn, "seed"); // mints the account + establishes the owner stream
        let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
        // A peer's node: present in the accepted-/3 projection, not yet materialized locally.
        seed_projected_node(&conn, stream, "mem_peer", "Invariant", "peer", "body", "active", &[]);
        let entries_before = content_entry_count(&conn);

        let outcome = drain_synced_stream_for_repo(&conn, REPO, 5_000).unwrap();
        assert_eq!(outcome.nodes_written, 1, "the peer node materializes");
        assert_eq!(origin_of(&conn, "mem_peer"), "synced");
        assert!(
            memory_by_id(&conn, "mem_peer").unwrap().is_some(),
            "searchable as a local row now"
        );

        // The reconcile runs: the synced row is excluded from re-authoring (origin gate), and it is
        // also already in the projection, so nothing is appended to the immutable /3 log.
        crate::memory_write::backfill_memory_oplog(&conn, 6_000).unwrap();
        assert_eq!(
            content_entry_count(&conn),
            entries_before,
            "a synced row is never re-authored into the signed /3 log",
        );
        assert_eq!(origin_of(&conn, "mem_peer"), "synced", "and it is never flipped to local");
    }

    // --- Task 9: the store-global drain the open/migrate seam calls ---

    /// The store-global entry (wired into the open/migrate seam) iterates every registered real
    /// repo and materializes its synced content, while leaving locally-authored rows untouched
    /// — proving the exact call the lifecycle open makes.
    #[test]
    fn the_store_global_drain_materializes_synced_content_and_spares_local_rows() {
        let conn = scoped_conn();
        // A real local memory mints the account + owner stream and projects itself origin='local'.
        let local_id = create_concept(&conn, "local seed");
        let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
        // A peer's node in the projection, not yet materialized locally.
        seed_projected_node(&conn, stream, "mem_peer", "Invariant", "peer", "body", "active", &[]);

        let outcome = drain_synced_streams_for_all_repos(&conn, 7_000).unwrap();
        assert_eq!(outcome.nodes_written, 1, "the peer node materializes for the registered repo");
        assert_eq!(origin_of(&conn, "mem_peer"), "synced");
        assert!(memory_by_id(&conn, "mem_peer").unwrap().is_some(), "readable as a local row");
        assert_eq!(origin_of(&conn, &local_id), "local", "the local row is left untouched");
    }

    #[test]
    fn content_then_production_anchors_surface_a_synced_memory_by_path() {
        let source = scoped_conn();
        let memory_id = create_concept(&source, "portable anchor");
        source
            .execute(
                "INSERT INTO repo_memory_bindings(
                     repo_id, memory_id, binding_kind, binding_id, path, start_line, end_line,
                     anchor_status, created_at_ms)
                 VALUES (?1, ?2, 'path', 'src/lib.rs', 'src/lib.rs', 3, 4, 'current', 1)",
                params![REPO, memory_id],
            )
            .unwrap();
        let account = rag_rat_oplog::local_account(&source, 1).unwrap();
        rag_rat_oplog::ensure_repo_incarnation(&source, REPO, 1).unwrap().unwrap();
        assert_eq!(rag_rat_oplog::table_sync_author_pending(&source, account, 2).unwrap(), 1);
        let route =
            rag_rat_oplog::table_sync_supported_streams(&source, account).unwrap().remove(0);

        let destination = scoped_conn();
        rag_rat_oplog::local_device(&destination, 0).unwrap();
        for entry in rag_rat_oplog::account_entries_for_sync(&source, account).unwrap() {
            rag_rat_oplog::account_ingest(&destination, &entry.signed_bytes, 0).unwrap();
        }
        rag_rat_oplog::adopt_local_account(
            &destination,
            account,
            rag_rat_oplog::read_local_account_genesis(&source).unwrap().unwrap(),
            0,
        )
        .unwrap();
        for entry in rag_rat_oplog::content_entries_for_sync(&source, account).unwrap() {
            rag_rat_oplog::content_ingest(&destination, &entry.signed_bytes, 1).unwrap();
        }
        let source_stream = rag_rat_oplog::owned_stream_v2_id(&source, REPO).unwrap().unwrap();
        let destination_stream = rag_rat_oplog::owned_stream_v2_id(&destination, REPO)
            .unwrap()
            .expect("account restore derives the repository's content stream");
        assert_eq!(destination_stream, source_stream);
        crate::drain_synced_memory(&destination).unwrap();
        assert!(memory_by_id(&destination, &memory_id).unwrap().is_some());
        assert!(
            memory::memories_for_path(&destination, "src/lib.rs", 10).unwrap().is_empty(),
            "content alone has no checkout-independent anchor",
        );

        let head = rag_rat_oplog::table_sync_chain_page_after(&source, account, &route, None, 10)
            .unwrap()
            .remove(0);
        for entry in rag_rat_oplog::table_sync_chain_entries(
            &source,
            account,
            &route,
            head.device_fingerprint,
            rag_rat_oplog::TableSyncEntryStart::Beginning,
            10,
        )
        .unwrap()
        {
            rag_rat_oplog::table_sync_ingest(
                &destination,
                account,
                &route,
                head.device_fingerprint,
                &entry.signed_bytes,
                3,
                None,
            )
            .unwrap();
        }
        let surfaced = memory::memories_for_path(&destination, "src/lib.rs", 10).unwrap();
        assert_eq!(surfaced.len(), 1);
        assert_eq!(surfaced[0].memory_id, memory_id);
    }

    /// Advance a stream's projection epoch WITHOUT rewriting the projection — the way to simulate
    /// "the projection changed" while keeping directly-seeded poison rows in place (a real
    /// reproject would rebuild them away). Mutates the internal `oplog_meta` epoch key by hand
    /// on purpose.
    fn bump_projection_epoch(conn: &Connection, stream: StreamId) {
        conn.execute(
            "UPDATE oplog_meta SET value = CAST(value AS INTEGER) + 1 WHERE key = \
             'content:proj-epoch:' || hex(?1)",
            params![stream.to_bytes().as_slice()],
        )
        .unwrap();
    }

    /// A concurrent `rag-rat rm` that commits its removal tombstone after the store-global drain
    /// has snapshotted the repo list must NOT re-materialize the removed repo's synced content.
    /// The in-transaction tombstone recheck skips it — without the guard, the projected peer
    /// node would be resurrected into `repo_memories` after `rm` reported it gone.
    #[test]
    fn a_drain_skips_a_repo_with_a_removal_tombstone() {
        let conn = scoped_conn();
        create_concept(&conn, "seed");
        let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();
        // A peer node in the projection that a drain WOULD materialize.
        seed_projected_node(&conn, stream, "mem_peer", "Invariant", "peer", "b", "active", &[]);
        // `rm` tombstones the repo (its purge cleared the rows; the drain must not put them back).
        rag_rat_db::schema::mark_repo_removed(&conn, REPO, 1).unwrap();

        let outcome = drain_synced_stream_for_repo(&conn, REPO, 2_000).unwrap();
        assert_eq!(outcome, DrainOutcome::default(), "a tombstoned repo's drain is a no-op");
        assert!(
            memory_by_id(&conn, "mem_peer").unwrap().is_none(),
            "the removed repo's synced content is not resurrected by the drain",
        );
    }

    /// The drain gate must SKIP its scan when the projection is unchanged since the last drain —
    /// and the skip must be load-bearing, not luck. Poison the projection with a node the scan
    /// WOULD materialize but WITHOUT advancing the epoch (a direct insert bypasses the
    /// reproject that bumps it); a gated re-drain must not pick it up. Then advance the epoch
    /// and confirm the SAME drain now does — proving the gate, not an empty projection, is what
    /// suppressed the first pass.
    #[test]
    fn the_drain_gate_skips_an_unchanged_projection_then_runs_when_the_epoch_advances() {
        let conn = scoped_conn();
        // Mints the account + owner stream and reprojects the local seed (epoch -> 1).
        create_concept(&conn, "seed");
        let stream = rag_rat_oplog::owned_stream_v2_id(&conn, REPO).unwrap().unwrap();

        // First drain records the watermark at the current epoch → nothing owed afterwards.
        drain_synced_streams_for_all_repos(&conn, 1_000).unwrap();
        assert!(
            !rag_rat_oplog::content_drain_needed(&conn, stream).unwrap(),
            "nothing is owed immediately after a drain",
        );

        // Poison: a projected node the scan WOULD materialize, inserted directly so the epoch does
        // NOT move (a real peer op would reproject and bump it).
        seed_projected_node(&conn, stream, "mem_poison", "Invariant", "p", "b", "active", &[]);

        // Gated re-drain: the epoch equals the watermark and nothing is pending, so the gate skips
        // the scan — the poison stays unseen.
        let skipped = drain_synced_streams_for_all_repos(&conn, 2_000).unwrap();
        assert_eq!(skipped, DrainOutcome::default(), "the gate skipped the scan");
        assert!(
            memory_by_id(&conn, "mem_poison").unwrap().is_none(),
            "a gated skip does not materialize a projected row the scan would have",
        );

        // Advance the epoch as a real reproject would; the SAME drain now runs and materializes it.
        bump_projection_epoch(&conn, stream);
        assert!(
            rag_rat_oplog::content_drain_needed(&conn, stream).unwrap(),
            "an epoch past the watermark is owed",
        );
        let ran = drain_synced_streams_for_all_repos(&conn, 3_000).unwrap();
        assert_eq!(ran.nodes_written, 1, "the drain runs once the epoch advances");
        assert!(
            memory_by_id(&conn, "mem_poison").unwrap().is_some(),
            "and materializes the row the earlier gated pass skipped",
        );
    }

    /// The public entry no-ops on an unstable repo id (legacy / local-only) — such an id can never
    /// root an owner stream, so there is nothing to drain and it must not touch the tables.
    #[test]
    fn the_public_entry_no_ops_on_an_unstable_repo_id() {
        let conn = scoped_conn();
        let legacy = rag_rat_base::repo_identity::LEGACY_REPO_ID;
        assert_eq!(
            drain_synced_stream_for_repo(&conn, legacy, 1_000).unwrap(),
            DrainOutcome::default(),
        );
        let local = format!("{}deadbeef", rag_rat_base::repo_identity::LOCAL_ONLY_ID_PREFIX);
        assert_eq!(
            drain_synced_stream_for_repo(&conn, &local, 1_000).unwrap(),
            DrainOutcome::default(),
        );
    }
}
