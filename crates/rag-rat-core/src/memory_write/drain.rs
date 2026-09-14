//! Draining accepted `/3` content into `repo_memories` / `repo_node_edges` — the REVERSE of the
//! local reconcile (`reconcile::reconcile_owner_stream_for_repo`, which authors local rows INTO the
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
//! same repo would delete the first's rows. A granted contributor (#1164) and a read-only
//! subscriber (#1156) therefore REPLACE the local derivation with the configured owner's stream
//! rather than draining both — and a repo may hold at most one of those two configurations.
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
use rag_rat_query::memory::{
    self, AppliedTarget, AppliedTargets, decode_applied_targets, encode_applied_targets,
};
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
/// condemn everything the owner's stream materialized. A read-only subscription (#1156) replaces it
/// for the same reason, with the extra consequence that a subscribed repo stops draining its own
/// account's stream: every `origin='synced'` row its SIBLING DEVICES put there is absent from the
/// owner's projection, so the next drain REMOVES it. That is why both setters (and both unsetters)
/// clear the OUTGOING stream's watermark as well as the incoming one — re-pointing back must
/// re-materialize what the re-point condemned rather than short-circuit on a watermark that is
/// still current.
///
/// Scope-gated the same way the reconcile is — a LEGACY placeholder or a `local:` shallow-clone id
/// can never root an owner stream, so both yield `None`.
pub(super) fn authoritative_content_stream(
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
    // A FOREIGN owner replaces the local derivation. Two configurations reach it, and a repo may
    // hold at most one (both setters refuse the other): a granted CONTRIBUTOR (#1164) reads back
    // the owner's stream — where its own writes went, and where the owner's and other contributors'
    // memories live — and a read-only SUBSCRIBER (#1156) mirrors a published owner without
    // authoring onto it. A configured owner that IS this store is neither; fall through to the
    // local derivation.
    // Lazily: a contributing repo must not pay for the subscription lookup on every drain, and a
    // corrupt `memory_subscription_owner` value must not error a drain that never consults it.
    let foreign_owner = match super::ownership::contribution_owner_account(conn, repo_id)? {
        Some(owner) => Some(owner),
        None => super::ownership::subscription_owner_account(conn, repo_id)?,
    };
    if let Some(owner) = foreign_owner
        && rag_rat_oplog::read_local_account(conn)? != Some(owner)
    {
        return verified_owner_stream(conn, repo_id, owner);
    }
    // Forward-derive the owner stream under the repo's access-mode intent — the SAME stream id the
    // live-write authored onto, so a published (PublicRead) repo drains its own public stream
    // rather than an empty Private one. `None` = no local account minted yet ⇒ nothing could
    // have been authored/ingested onto this stream ⇒ nothing to drain (the analog of an
    // unstable scope).
    let mode = super::ownership::owner_stream_access_mode(conn, repo_id)?;
    rag_rat_oplog::owned_stream_v2_id_with_mode(conn, repo_id, mode)
}

/// `owner`'s stream for `repo_id`, but ONLY once the ownership fact has folded here.
///
/// A DERIVED stream id is not yet an authority. Both `sync contribute` and `sync subscribe`
/// deliberately succeed before the owner's log is synced (configure, then sync), and a mistyped
/// owner id derives a stream that will never exist at all — in both cases the projection is EMPTY,
/// and handing that to the drain would make the removal anti-joins condemn every synced row the
/// repo currently reads. So authority begins only once the ownership fact has folded here. Until
/// then this repo drains NOTHING: `None` rather than falling through to the local stream, whose own
/// empty projection would condemn exactly the same rows.
///
/// Both configurations target the owner's PublicRead stream (v1 public only).
fn verified_owner_stream(
    conn: &Connection,
    repo_id: &str,
    owner: rag_rat_oplog::AccountId,
) -> anyhow::Result<Option<StreamId>> {
    let stream = rag_rat_oplog::owner_stream_v2_id_for_account(
        repo_id,
        owner,
        rag_rat_oplog::AccessMode::PublicRead,
    )?;
    if rag_rat_oplog::stream_owner_account(conn, stream)? != Some(owner) {
        return Ok(None);
    }
    Ok(Some(stream))
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
    // Whose anchor sets `anchors/1` already carries here: the local account's. Read once per pass.
    let local_account = rag_rat_oplog::read_local_account(tx)?;
    for node in rag_rat_oplog::list_projected_content_nodes(tx, stream)? {
        match drain_node(tx, repo_id, &node, local_account.as_ref(), now_ms)? {
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

/// Whether a projected node passes the SAME validity gates the local create/update path enforces —
/// kind / confidence closed sets, title / body length caps, source, and the kind↔payload rule
/// (`validate_payload`). Peer content crosses the wire only SHAPE-validated, and an older or
/// compromised account device could author content that clears the §18a envelope cap yet violates
/// these tighter local rules; such a node must not be persisted into the searchable tables.
///
/// The published source hash is deliberately NOT gated here. Failing this function quarantines the
/// whole memory — deleting its row, bindings and FTS shadow — and the projection re-offers the same
/// value forever, so a peer on a newer digest shape would cost every older peer the memory itself.
/// Its shape is filtered at the one place that stores it (`published_source_hash`) instead: the
/// same guarantee that nothing malformed is persisted, without the blast radius.
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

/// The exact shape `hex_sha256` produces — the only value the local write path ever stores in
/// `repo_memories.source_text_hash`.
fn is_hex_sha256(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
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
    local_account: Option<&rag_rat_oplog::AccountId>,
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
            // First sight of this node — received from a peer. `created_by` / `input_hash` have no
            // op home and are nullable; `memory_version` is the author-side constant; the clock is
            // bookkeeping only. `origin='synced'` is what the authoring gate keys off.
            //
            // `source_text_hash` DOES have an op home, but it is not written here: it is a claim
            // ABOUT an anchor set, so it is applied beside one (see `apply_published_anchors`).
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
    // Apply AFTER materialization, and only for a node that belongs to THIS repo — the sibling-repo
    // arm above must not touch that repo's bindings any more than it touches its content. The
    // snapshot check comes first because it is free: until anchors are authored, every node answers
    // `None` and this costs no query at all.
    if node.anchors.is_some() && node_in_repo(tx, &node.node_id, repo_id)? {
        // A memory that was condemned or quarantined here and has since returned kept its
        // bindings; give it back the baseline parked with them, ahead of the snapshot read below,
        // so its author's rebinds since then land instead of being recorded as applied. Done here,
        // on a pass that carries anchors, rather than on the insert: a memory can return ahead of
        // its anchors, and a baseline restored then would be consumed with nothing to act on it.
        let returned = restore_parked_anchor_baseline(tx, repo_id, &node.node_id)?;
        // A memory created here takes a published set by the same provenance rules as a synced one
        // once its author is known: this account's own set only fills an empty memory and brings
        // its hash (a sibling device's rebind reaches this one's rows through `anchors/1`, but the
        // hash only through the snapshot), and another account's set — a contributor rebinding a
        // memory this account created — reaches it through the snapshot alone. With no author
        // known it only ever seeds.
        let changed = match applied_snapshot(tx, repo_id, &node.node_id)? {
            Some(applied) if !applied.local || node.anchors_author.is_some() =>
                apply_published_anchors(tx, repo_id, node, local_account, AppliedSnapshot {
                    returned,
                    ..applied
                })?,
            _ => seed_node_anchors(tx, repo_id, node)? > 0,
        };
        if changed {
            // `anchors/1` declares that a write to this table advances these lanes, and the `/5`
            // applier bumps them for exactly this reason. The seed is a second writer to the same
            // table, and on the converged path no `repo_memories` row is touched — so the row
            // triggers that normally carry the lanes never fire, and without this a reader's Lens
            // view keeps serving a revision that predates the bindings.
            if rag_rat_db::schema::repo_id_is_registered(tx, repo_id)? {
                rag_rat_db::meta::bump_lens_revisions(tx, repo_id, &[
                    rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
                    rag_rat_db::meta::LENS_MEMORIES_REVISION_META,
                ])?;
            }
        }
    }
    Ok(effect)
}

/// The binding kinds the local write path can produce, and therefore the only ones worth seeding
/// from a peer's snapshot. An unknown kind is a newer peer's vocabulary: it means nothing here, so
/// it is skipped row-wise rather than quarantining the node over its decoration.
///
/// Two kinds the local path DOES produce are deliberately absent, both because a seeded row could
/// never resolve here and would sit `unverified` forever:
/// - `call_path`, whose supporting `repo_memory_call_paths` / `_edges` rows are in no replication
///   scope, so the path it names does not exist locally.
/// - `chunk`, whose `binding_id` IS a checkout-local rowid — reassigned on every re-chunk, so a
///   peer's integer is meaningless here. Its validator also short-circuits `unverified` whenever
///   the local `chunk_id` column is NULL, which is exactly what a seeded row leaves it as, so the
///   hash-relocation fallback that might have rescued it is unreachable.
///
/// Everything remaining is portable by construction: qualified names with name-based relocation,
/// an edge fingerprint, or a plain string.
const SEEDABLE_BINDING_KINDS: &[&str] =
    &["logical_symbol", "symbol", "edge", "scip_moniker", "path", "dir", "commit", "tracker"];

/// Seed a LOCAL memory's bindings from a peer's anchor snapshot — ONLY when this store holds none
/// for it: an unbound memory created here and rebound on another device of the account. A synced
/// memory goes through [`apply_published_anchors`] instead, which can also replace.
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
    insert_seedable_anchors(tx, repo_id, &node.node_id, anchors)
}

/// Insert the anchors of a published snapshot that this store can resolve, as portable columns
/// only.
fn insert_seedable_anchors(
    tx: &Transaction<'_>,
    repo_id: &str,
    node_id: &str,
    anchors: &[rag_rat_oplog::PortableAnchor],
) -> anyhow::Result<usize> {
    let mut seeded = 0;
    for anchor in anchors {
        if !SEEDABLE_BINDING_KINDS.contains(&anchor.binding_kind.as_str()) {
            // A deliberately-excluded kind is the ordinary case, not an anomaly — a call-path-bound
            // memory reaching a peer is normal — and the projection re-offers it on every pass, so
            // warning here would repeat forever for a decision this store already made.
            tracing::debug!(
                repo_id,
                node_id,
                binding_kind = %anchor.binding_kind,
                "not seeding an anchor of a kind this store cannot resolve",
            );
            continue;
        }
        if anchor.binding_id.is_empty() {
            tracing::warn!(
                repo_id,
                node_id,
                binding_kind = %anchor.binding_kind,
                "skipping an anchor with an empty binding id: it would make a degenerate primary \
                 key",
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
                node_id,
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

/// What the drain last applied to a memory of this repo, or `None` when it has no row here.
struct AppliedSnapshot {
    /// Whether the memory was created here (`origin = 'local'`): the bindings it holds are its
    /// creator's own last set, and it only seeds from a set whose author is unknown.
    local: bool,
    /// The [`anchor_snapshot_digest`] of the set last applied; NULL until one is.
    digest: Option<String>,
    /// The published hash last applied, as stored; NULL until one is, or when it was none.
    hash: Option<String>,
    /// What that set named for each symbol anchor (see [`encode_applied_targets`]); NULL until a
    /// set is applied.
    targets: Option<String>,
    /// Whether the row was materialized again this pass with a parked baseline restored (see
    /// [`restore_parked_anchor_baseline`]): the set last applied is known, but the hash beside it
    /// is the one the row carried away, so the stamp is reconsidered whatever the author published
    /// since — a hash withdrawn while the memory was away must not come back with it.
    returned: bool,
}

fn applied_snapshot(
    tx: &Transaction<'_>,
    repo_id: &str,
    memory_id: &str,
) -> anyhow::Result<Option<AppliedSnapshot>> {
    tx.query_row(
        "SELECT anchors_applied_digest, source_hash_applied, anchors_applied_targets,
                origin = 'local'
           FROM repo_memories
         WHERE id = ?1 AND repo_id = ?2",
        params![memory_id, repo_id],
        |row| {
            Ok(AppliedSnapshot {
                local: row.get(3)?,
                digest: row.get(0)?,
                hash: row.get(1)?,
                targets: row.get(2)?,
                returned: false,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Apply the anchor set and source hash a synced memory's author published — or, for a memory
/// created here, a set another account authored — wherever either CHANGED since the drain last
/// applied it. Returns whether the memory's bindings or hash moved.
///
/// Change, not difference, is the trigger. The bindings here drift from the snapshot by design —
/// the validate/relocate loop re-keys them as the checkout moves — so comparing against them would
/// undo that on every pass. The applied digest moves only when the AUTHOR publishes a new set, and
/// then that set wins (by identity — see [`converge_bindings`]): it is how a rebind reaches a
/// receiver already holding the old bindings.
///
/// Bindings present with no digest recorded arrived some other way — `anchors/1` from a sibling
/// device, or a seed from before the digest existed — and are some device's image of SOME
/// publication. Which one decides what happens to them (see [`held_is_superseded_image`]): the
/// current publication's image is recorded as applied and left alone; the image of a publication
/// another account made and the fold has superseded — a device joining the account, or re-syncing
/// after a purge, holding rows a sibling parked a baseline for but it never held (#1304) — is
/// converged, judged against the rows' own authored kind and signature; rows matching no such
/// publication — a sibling's rebind or republication whose own set has yet to fold here, an older
/// binary's relocation — are recorded and left for the set that explains them. A set the local
/// account authored, or one the fold could not attribute, is never converged here. The author's
/// hash lands only on rows that match the published set by target, checked on every pass that
/// would stamp, so a later hash-only pass keeps it off rows that are not the author's.
///
/// The hash is applied when it or the set changes, so a hash published for a set this store already
/// holds lands without touching the bindings. That pairs safely because a rebind always publishes
/// the hash beside the anchors — an explicit empty one when the new target has none (see
/// `author_anchors`) — so the register never outlives the set it describes. Every change is
/// recorded in `source_hash_applied`, apart from the stamped `source_text_hash`, so a rebind made
/// here keeps its own hash until the author publishes something new — even one made after a hash
/// arrived while no binding was held. A memory holding no binding is left unstamped: the hash would
/// describe nothing there.
///
/// Which account authored the set decides how far this goes. A set this store's own account
/// authored also reaches it through `anchors/1`, the carrier of every later rebind and relocation
/// of it, so converging here would race that carrier: the own path writes bindings only into a
/// memory holding none, and stamps the published hash. A set another account authored arrives
/// through the snapshot alone and converges. A receiver with several devices shares the converged
/// rows between them through its own `anchors/1`, and each converges to the same snapshot sequence.
fn apply_published_anchors(
    tx: &Transaction<'_>,
    repo_id: &str,
    node: &ProjectedContentNode,
    local_account: Option<&rag_rat_oplog::AccountId>,
    applied: AppliedSnapshot,
) -> anyhow::Result<bool> {
    let Some(anchors) = node.anchors.as_deref() else {
        return Ok(false);
    };
    // A set this store's own account authored reaches it through `anchors/1` too, the carrier of
    // every later rebind and relocation of it; converging to the snapshot would race that carrier,
    // so the own path writes bindings only into a memory holding none. A set another account
    // authored reaches this store through the snapshot alone.
    let own = node.anchors_author.is_some() && node.anchors_author.as_ref() == local_account;
    let mut changed = false;
    let digest = anchor_snapshot_digest(&node.node_id, anchors);
    let set_changed = applied.digest.as_deref() != Some(digest.as_str());
    let held = super::authoring::portable_anchors_of(tx, &node.node_id)?;
    // A memory holding no binding takes the published set whether or not it changed: an author
    // cannot publish an unbinding (a rebind needs a target), so an empty memory under a recorded
    // set lost its rows some other way — a sibling device's re-point, or an older binary's
    // removal, reaching it through `anchors/1` — and seeding into that vacuum is what heals it.
    if held.is_empty() && !set_changed {
        changed |= converge_bindings(tx, repo_id, &node.node_id, anchors, &held, None, &[])?;
    }
    if set_changed {
        // Bindings held with no digest recorded converge only when they are the image of a
        // publication the fold has superseded (`held_is_superseded_image`): the current image is
        // recorded as it is, rows of no known publication wait for the set that explains them. A
        // local memory's held bindings are its creator's own last set, which another account's set
        // supersedes on first sight.
        if held.is_empty()
            || (!own
                && (applied.digest.is_some()
                    || applied.local
                    || (node.anchors_author.is_some()
                        && held_is_superseded_image(
                            &held,
                            anchors,
                            &node.superseded_anchors,
                            local_account,
                        ))))
        {
            let previous = decode_applied_targets(applied.targets.as_deref());
            let scopes = published_anchor_scopes(repo_id, node);
            changed |= converge_bindings(
                tx,
                repo_id,
                &node.node_id,
                anchors,
                &held,
                previous.as_ref(),
                &scopes,
            )?;
        }
        // The baseline a later set is judged against holds only anchors a held row now matches by
        // target. A set recorded against rows that are not its own — a stale seed, a local rebind,
        // a row `anchors/1` has yet to move — must not vouch for them: the author's next republish
        // would equal the baseline and a retarget those rows still await would go unmarked.
        let held_now = super::authoring::portable_anchors_of(tx, &node.node_id)?;
        let matched: Vec<rag_rat_oplog::PortableAnchor> = anchors
            .iter()
            .filter(|anchor| held_now.iter().any(|row| same_target(row, anchor)))
            .cloned()
            .collect();
        let targets = encode_applied_targets(&applied_targets_of(
            &matched,
            &published_anchor_scopes(repo_id, node),
        ))?;
        tx.execute(
            "UPDATE repo_memories SET anchors_applied_digest = ?3, anchors_applied_targets = ?4
             WHERE id = ?1 AND repo_id = ?2",
            params![node.node_id, repo_id, digest, targets],
        )?;
    } else if let Some(mut targets) = decode_applied_targets(applied.targets.as_deref()) {
        // The scopes paired with a set can move without the set's bytes changing: an upgrade
        // re-folds the `node_anchor_scopes` ops an older binary retained opaque, and an author
        // upgrading republishes its unchanged set with scopes beside it (the sweep in
        // `read_anchor_backfill_ids`). The baseline follows the paired scopes — a missing one is
        // filled in, so the author's next rebind between twins has a recorded scope to differ
        // from; a withdrawn one is cleared; a changed one replaces the old — and marks nothing: a
        // rebind always changes the set's bytes, so a scope moving under identical ones is a
        // change of derivation, not of target, and a mark would hold back every such row at once.
        // Only for anchors a held row still matches by target, as the set-changed path records
        // its baseline: a row relocated here since must not have a scope recorded on its behalf.
        let scopes = published_anchor_scopes(repo_id, node);
        let held_now = super::authoring::portable_anchors_of(tx, &node.node_id)?;
        let mut moved = false;
        for ((kind, id), target) in targets.iter_mut() {
            let matched = anchors.iter().any(|anchor| {
                anchor.binding_kind == *kind
                    && anchor.binding_id == *id
                    && held_now.iter().any(|row| same_target(row, anchor))
            });
            if !matched {
                continue;
            }
            let published = scopes
                .iter()
                .find(|(k, i, _)| k == kind && i == id)
                .map(|(_, _, scope)| scope.clone());
            if target.scope_hash != published {
                target.scope_hash = published;
                moved = true;
            }
        }
        if moved {
            tx.execute(
                "UPDATE repo_memories SET anchors_applied_targets = ?3
                 WHERE id = ?1 AND repo_id = ?2",
                params![node.node_id, repo_id, encode_applied_targets(&targets)?],
            )?;
        }
    }
    let published = published_source_hash(repo_id, node);
    // Reconsidered when the SET changes, not only the hash: a new set can give an unchanged hash
    // its first binding. A memory bound only to a chunk (never seeded here) takes no stamp, and a
    // rebind to the symbol over the same text republishes the same hash — gating on the hash alone
    // would leave that memory without one for good. A memory materialized again with its parked
    // baseline is reconsidered too: the hash on it is the one it carried away, whether or not the
    // author still publishes one.
    if set_changed || applied.returned || published != applied.hash.as_deref() {
        // Stamped only beside the author's bindings: every row held must be one the published set
        // names, with the same target. Checked on EVERY pass that would stamp, a hash-only one
        // included — a pull can deliver the author's new hash ahead of its set — and by target,
        // never by kind: a chunk rebound here is not the author's chunk. A memory holding no
        // binding is left unstamped, since the hash would describe nothing there.
        let held = super::authoring::portable_anchors_of(tx, &node.node_id)?;
        // On the own path the held rows are this account's own, converging to the same rebind
        // through `anchors/1`, so the hash lands even while they catch up.
        if own || held.iter().all(|row| anchors.iter().any(|anchor| same_target(row, anchor))) {
            let stamp = published.filter(|_| !held.is_empty());
            changed |= tx.execute(
                "UPDATE repo_memories SET source_text_hash = ?3
                 WHERE id = ?1 AND repo_id = ?2 AND source_text_hash IS NOT ?3",
                params![node.node_id, repo_id, stamp],
            )? > 0;
        }
        tx.execute(
            "UPDATE repo_memories SET source_hash_applied = ?3 WHERE id = ?1 AND repo_id = ?2",
            params![node.node_id, repo_id, published],
        )?;
    }
    Ok(changed)
}

/// Whether a held binding and a published anchor name the same row — `(binding_kind, binding_id)`,
/// the primary key.
fn same_row(held: &rag_rat_oplog::PortableAnchor, anchor: &rag_rat_oplog::PortableAnchor) -> bool {
    held.binding_kind == anchor.binding_kind && held.binding_id == anchor.binding_id
}

/// Whether a held binding and a published anchor name the same row AND the same target, compared on
/// what identifies the target rather than where it currently sits. The row alone is not enough: a
/// struct and its impl share a qualified name, told apart by `symbol_kind` and `signature_hash`.
/// The location is left out because it says where the author found the target, not which target:
/// two captures of one symbol can differ on `path`, the line span and `moniker_tool_version`
/// without the target having changed — and `created_at_ms` because each rebind restamps it.
///
/// A kind the drain never installs (`chunk`, `call_path`) never matches: its id is a checkout-local
/// rowid, so equal ids on two stores say nothing about the target, and such a row held beside a
/// foreign set can only be a rebind made here — which keeps the author's hash off it.
fn same_target(
    held: &rag_rat_oplog::PortableAnchor,
    anchor: &rag_rat_oplog::PortableAnchor,
) -> bool {
    SEEDABLE_BINDING_KINDS.contains(&held.binding_kind.as_str())
        && same_row(held, anchor)
        && held.symbol_kind == anchor.symbol_kind
        && held.signature_hash == anchor.signature_hash
        && held.moniker_tool == anchor.moniker_tool
        && held.commit_hash == anchor.commit_hash
        && held.tracker == anchor.tracker
        && held.project == anchor.project
        && held.item_key == anchor.item_key
}

/// Whether the held rows are the published set's image — what this store would hold had it
/// converged on exactly this publication: every seedable held row names a published anchor's target
/// under the same `created_at_ms`, and every anchor this store would install is held. The stamp is
/// the publication: a rebind restamps every row with one clock value, `anchors/1` carries it
/// verbatim and no local write touches it, so it tells one publication's image from a republish of
/// the same targets, which the target columns cannot (`ANCHOR_MATCHES_BINDING_SQL` reads it the
/// same way). Kinds this store never installs are outside the comparison, as they are outside
/// [`converge_bindings`].
fn held_is_published_image(
    held: &[rag_rat_oplog::PortableAnchor],
    anchors: &[rag_rat_oplog::PortableAnchor],
) -> bool {
    let seedable = |anchor: &&rag_rat_oplog::PortableAnchor| {
        SEEDABLE_BINDING_KINDS.contains(&anchor.binding_kind.as_str())
    };
    held.iter().filter(seedable).all(|row| {
        anchors
            .iter()
            .any(|anchor| same_target(row, anchor) && row.created_at_ms == anchor.created_at_ms)
    }) && anchors
        .iter()
        .filter(seedable)
        .filter(|anchor| !anchor.binding_id.is_empty())
        .all(|anchor| held.iter().any(|row| same_row(row, anchor)))
}

/// Whether rows held with no applied set are the image of a publication ANOTHER account made and
/// the fold has SUPERSEDED (`superseded`, see `ProjectedContentNode::superseded_anchors`) — and not
/// of the published set itself — so converging them on the published set brings them up to date
/// without undoing anything. Rows matching no such publication are not touched: they may be a
/// sibling's rebind whose own set has yet to fold here, and overwriting them would republish the
/// older rows to every device of the account through `anchors/1`, with nothing left to restore the
/// rebind once its set arrives as this account's own. A superseded publication this account made
/// is excluded for the same reason: a sibling's later republication of the unchanged set keeps the
/// same stamps, so the image alone cannot tell the two apart, and that later one arrives as this
/// account's own too. Another account's republication of one of its own sets arrives as a set
/// change and converges. The match is by publication, the stamp included, and needs at least one
/// seedable held row: kinds this store never installs are outside it, and a set of them alone is no
/// image of anything.
fn held_is_superseded_image(
    held: &[rag_rat_oplog::PortableAnchor],
    anchors: &[rag_rat_oplog::PortableAnchor],
    superseded: &[rag_rat_oplog::SupersededAnchorSet],
    local_account: Option<&rag_rat_oplog::AccountId>,
) -> bool {
    held.iter().any(|row| SEEDABLE_BINDING_KINDS.contains(&row.binding_kind.as_str()))
        && !held_is_published_image(held, anchors)
        && superseded.iter().any(|set| {
            set.author.is_some()
                && set.author.as_ref() != local_account
                && held_is_published_image(held, &set.anchors)
        })
}

/// The identity of a published anchor set: its byte-canonical op encoding, hashed. The fold has
/// already sorted the set, so every device folding the same register records the same digest. The
/// encoding is frozen — the op golden vectors pin it, and a new portable column has to become a new
/// op kind — so a digest one release recorded still matches under the next, and an upgrade never
/// reads as a republish.
fn anchor_snapshot_digest(node_id: &str, anchors: &[rag_rat_oplog::PortableAnchor]) -> String {
    rag_rat_base::hash::hex_sha256(&rag_rat_oplog::encode_op(
        &rag_rat_oplog::MemoryOp::NodeAnchors {
            node_id: rag_rat_oplog::NodeId::from(node_id),
            anchors: anchors.to_vec(),
        },
    ))
}

/// What a published set names for each symbol anchor — the kind and signature the anchor carries,
/// and the scope its `node_anchor_scopes` companion carries for it — as the baseline a later set is
/// judged against.
fn applied_targets_of(
    anchors: &[rag_rat_oplog::PortableAnchor],
    scopes: &[(String, String, String)],
) -> AppliedTargets {
    anchors
        .iter()
        .filter(|anchor| matches!(anchor.binding_kind.as_str(), "symbol" | "logical_symbol"))
        .map(|anchor| {
            let scope_hash = scopes
                .iter()
                .find(|(kind, id, _)| *kind == anchor.binding_kind && *id == anchor.binding_id)
                .map(|(_, _, scope)| scope.clone());
            ((anchor.binding_kind.clone(), anchor.binding_id.clone()), AppliedTarget {
                symbol_kind: anchor.symbol_kind.clone(),
                signature_hash: anchor.signature_hash.clone(),
                scope_hash,
            })
        })
        .collect()
}

/// The published anchor scopes this store will record: each a lowercase-hex sha256, as
/// `(binding_kind, binding_id, scope_hash)` for [`encode_applied_targets`]. An off-shape value is a
/// peer's this binary cannot read and is dropped here, where it would be stored — never in the
/// content gate, for the same reason as the hash.
fn published_anchor_scopes(
    repo_id: &str,
    node: &ProjectedContentNode,
) -> Vec<(String, String, String)> {
    node.anchor_scopes
        .iter()
        .filter_map(|((kind, id), scope_hash)| {
            if is_hex_sha256(scope_hash) {
                return Some((kind.clone(), id.clone(), scope_hash.clone()));
            }
            tracing::debug!(
                repo_id,
                node_id = %node.node_id,
                binding_kind = %kind,
                "not recording a published anchor scope outside the lowercase-hex-sha256 shape",
            );
            None
        })
        .collect()
}

/// The published hash as this store holds it: a lowercase-hex sha256, or `None`. An empty string is
/// an author's explicit retraction. Any other off-shape value is a peer's this binary cannot read,
/// so it is dropped — here, where it would be stored, and not in the content gate, where failing
/// quarantines the whole memory over a decoration.
fn published_source_hash<'a>(repo_id: &str, node: &'a ProjectedContentNode) -> Option<&'a str> {
    let hash = node.source_text_hash.as_deref().filter(|hash| !hash.is_empty())?;
    if is_hex_sha256(hash) {
        return Some(hash);
    }
    tracing::debug!(
        repo_id,
        node_id = %node.node_id,
        "not stamping a published source hash outside the lowercase-hex-sha256 shape",
    );
    None
}

/// Bring a memory's bindings to a published set BY IDENTITY — `(binding_kind, binding_id)`, the
/// primary key — for the kinds this store can install. A held row the set no longer names is
/// deleted. A named row takes the author's values and drops its cached resolution for the validate
/// loop to redo — on every set change, not only when its portable columns differ: `anchors/1`
/// updates those in place and keeps the local ids, so a row whose target moved under an unchanged
/// identity (a struct and its impl share a qualified name) can match the new set while its ids
/// still name the old target. A named anchor missing here is inserted. Returns whether any row
/// moved.
///
/// Kinds this store never inserts (`chunk`, `call_path`) are never deleted or refreshed. The drain
/// cannot put back what it removes of them, and on a device of the author's own account they are
/// `anchors/1`'s to carry: a chunk binding relocates on every re-chunk without a new snapshot, so
/// deleting the unnamed, relocated row would unbind the memory — and `anchors/1`, which publishes
/// this table's deletes, would carry that to every device, the author's included. Their
/// checkout-local id is also their whole resolution, which a refresh would clear for good.
fn converge_bindings(
    tx: &Transaction<'_>,
    repo_id: &str,
    memory_id: &str,
    anchors: &[rag_rat_oplog::PortableAnchor],
    held: &[rag_rat_oplog::PortableAnchor],
    previous: Option<&AppliedTargets>,
    scopes: &[(String, String, String)],
) -> anyhow::Result<bool> {
    let mut changed = false;
    for row in held.iter().filter(|row| SEEDABLE_BINDING_KINDS.contains(&row.binding_kind.as_str()))
    {
        match anchors.iter().find(|anchor| same_row(row, anchor)) {
            Some(anchor) => {
                let previous = previous
                    .and_then(|targets| {
                        targets.get(&(row.binding_kind.clone(), row.binding_id.clone()))
                    })
                    .cloned();
                let published_scope = scopes
                    .iter()
                    .find(|(kind, id, _)| *kind == row.binding_kind && *id == row.binding_id)
                    .map(|(_, _, scope)| scope.as_str());
                refresh_binding(tx, repo_id, memory_id, anchor, previous, published_scope)?
            },
            None => {
                tx.execute(
                    "DELETE FROM repo_memory_bindings
                     WHERE repo_id = ?1 AND memory_id = ?2 AND binding_kind = ?3
                       AND binding_id = ?4",
                    params![repo_id, memory_id, row.binding_kind, row.binding_id],
                )?;
            },
        }
        changed = true;
    }
    let missing: Vec<rag_rat_oplog::PortableAnchor> = anchors
        .iter()
        .filter(|anchor| !held.iter().any(|row| same_row(row, anchor)))
        .cloned()
        .collect();
    if insert_seedable_anchors(tx, repo_id, memory_id, &missing)? > 0 {
        changed = true;
    }
    Ok(changed)
}

/// Give a held row the author's portable values for its target, and clear the cached resolution the
/// validator would otherwise trust outright — the raw symbol and chunk ids and the verdict.
///
/// An edge's `edge_id` is KEPT: its binding id is the edge fingerprint, which names the exact edge
/// (resolved callee included), so a row the set still names is still that edge. Clearing it would
/// lose an edge only a linked worktree holds — validation keeps such a sibling edge pending only
/// while its stored id is present.
///
/// The logical handle is KEPT: it is the only stored evidence that tells two impls of different
/// traits for one type apart when they also share the captured signature, so clearing it would
/// send the binding to the lowest-id twin. But it can name the target the author just left — a
/// rebind between two impls of one type keeps the binding's identity and kind — so a symbol row
/// whose published kind or signature differs from what the author published for it last time
/// (`previous`, from the last applied set; the row's own values when none was recorded) is marked
/// [`rag_rat_query::memory::RelocationReason::Retargeted`], and an earlier mark still unanswered is
/// kept. The validator, running scoped to a checkout of the memory's repo (which this drain,
/// running for every repo, is not), then weighs the author's evidence above the handle.
///
/// The baseline is the author's last statement, not the row: the row's authored kind and signature
/// are the author's too, but a row seeded from an older set, or converged on a foreign one, can
/// carry values the current author never published, and an unchanged republish would then read as
/// a retarget and the validator follow the old signature to a same-named sibling.
///
/// The target's published scope (`published_scope`, see `rag_rat_oplog::AnchorScope`) is judged the
/// same way, against the scope the baseline recorded — and only where BOTH are known. Two impls of
/// different traits for one type can agree on the kind and the captured signature, so the scope is
/// what says a rebind between them moved (#1276). A scope missing on either side is no evidence:
/// an older author publishes none, and marking on its absence would retarget every symbol row on
/// the first publication after an upgrade.
pub(crate) fn refresh_binding(
    tx: &Transaction<'_>,
    repo_id: &str,
    memory_id: &str,
    anchor: &rag_rat_oplog::PortableAnchor,
    previous: Option<AppliedTarget>,
    published_scope: Option<&str>,
) -> anyhow::Result<()> {
    let held: Option<(Option<String>, Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT symbol_kind, signature_hash, relocation_reason FROM repo_memory_bindings
             WHERE repo_id = ?1 AND memory_id = ?2 AND binding_kind = ?3 AND binding_id = ?4",
            params![repo_id, memory_id, anchor.binding_kind, anchor.binding_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let retarget = rag_rat_query::memory::RelocationReason::Retargeted.as_db_str();
    let retargeted = matches!(anchor.binding_kind.as_str(), "symbol" | "logical_symbol")
        && held.is_some_and(|(kind, signature, reason)| {
            let previous = previous.unwrap_or(AppliedTarget {
                symbol_kind: kind,
                signature_hash: signature,
                scope_hash: None,
            });
            let scope_moved = match (previous.scope_hash.as_deref(), published_scope) {
                (Some(recorded), Some(published)) => recorded != published,
                _ => false,
            };
            reason.as_deref() == Some(retarget)
                || previous.symbol_kind != anchor.symbol_kind
                || previous.signature_hash != anchor.signature_hash
                || scope_moved
        });
    tx.execute(
        "UPDATE repo_memory_bindings
         SET path = ?5, start_line = ?6, end_line = ?7, commit_hash = ?8, tracker = ?9,
             project = ?10, item_key = ?11, symbol_kind = ?12, signature_hash = ?13,
             moniker_tool = ?14, moniker_tool_version = ?15, created_at_ms = ?16,
             symbol_id = NULL, chunk_id = NULL,
             resolved = NULL, resolved_binding_id = NULL, resolved_path = NULL,
             resolved_start_line = NULL,
             resolved_end_line = NULL, resolved_symbol_kind = NULL,
             resolved_signature_hash = NULL, resolved_moniker_tool_version = NULL,
             anchor_status = 'unverified',
             relocation_reason = ?17,
             downgrade_pending_at_ms = NULL
         WHERE repo_id = ?1 AND memory_id = ?2 AND binding_kind = ?3 AND binding_id = ?4",
        params![
            repo_id,
            memory_id,
            anchor.binding_kind,
            anchor.binding_id,
            anchor.path,
            anchor.start_line,
            anchor.end_line,
            anchor.commit_hash,
            anchor.tracker,
            anchor.project,
            anchor.item_key,
            anchor.symbol_kind,
            anchor.signature_hash,
            anchor.moniker_tool,
            anchor.moniker_tool_version,
            anchor.created_at_ms,
            retargeted.then_some(retarget),
        ],
    )?;
    Ok(())
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
/// The contentless FTS shadow has no parent FK, so it is deleted explicitly before the parent; the
/// tag / call-path / edge children cascade. The bindings are KEPT (see
/// [`park_anchor_baselines`]): every user-facing reader joins `repo_memories`, so a binding whose
/// memory is gone is invisible, and deleting it would publish a `Remove` on `anchors/1` to every
/// device of the account, the ones where the memory is still live included (#1298).
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
    park_anchor_baselines(tx, CONDEMNED, params![repo_id, stream.to_bytes().as_slice()])?;
    let removed = tx.execute(
        "DELETE FROM repo_memories
         WHERE repo_id = ?1 AND origin = 'synced'
           AND id NOT IN (SELECT node_id FROM content_projected_nodes WHERE stream_id = ?2)",
        params![repo_id, stream.to_bytes().as_slice()],
    )?;
    Ok(removed as u32)
}

/// Drop an existing `origin='synced'` mirror of `node_id` under `repo_id` (FTS shadow first — it
/// has no FK to cascade; the other children cascade, and the bindings are KEPT with their baseline
/// parked, as [`remove_vanished_synced_nodes`] keeps them). Used
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
    park_anchor_baselines(
        tx,
        "SELECT id FROM repo_memories WHERE id = ?1 AND repo_id = ?2 AND origin = 'synced'",
        params![node_id, repo_id],
    )?;
    let removed = tx.execute(
        "DELETE FROM repo_memories WHERE id = ?1 AND repo_id = ?2 AND origin = 'synced'",
        params![node_id, repo_id],
    )?;
    Ok(removed > 0)
}

/// Park the applied-anchor baseline of every memory `condemned` selects (a `SELECT id` over
/// `repo_memories`, taking `params`), ahead of deleting their rows. The bindings those rows leave
/// behind replicate on `anchors/1` and stay; the baseline — which set the drain last applied, and
/// what that set named per anchor — lived on the row, and without it a returning memory would take
/// its held bindings for the published set and never converge on a rebind made while it was away.
/// The memory's own `source_text_hash` is parked with it: the stamp lands only beside bindings that
/// match the published set by target, so a row relocated here would return without one for good.
/// The applied HASH is deliberately left behind: the returning row reads it as absent, which is
/// what makes [`apply_published_anchors`] run the stamp again (the INSERT leaves the column
/// `NULL`) — on a return with the set unchanged included, where nothing else would. Only a memory
/// with an applied set has anything to park.
fn park_anchor_baselines(
    tx: &Transaction<'_>,
    condemned: &str,
    params: &[&dyn rusqlite::ToSql],
) -> anyhow::Result<()> {
    tx.execute(
        &format!(
            "INSERT OR REPLACE INTO repo_memory_parked_baselines(
                 repo_id, memory_id, anchors_applied_digest, anchors_applied_targets,
                 source_text_hash)
             SELECT repo_id, id, anchors_applied_digest, anchors_applied_targets, source_text_hash
               FROM repo_memories
              WHERE id IN ({condemned}) AND anchors_applied_digest IS NOT NULL"
        ),
        params,
    )?;
    Ok(())
}

/// Give a synced memory the drain has materialized again the baseline parked when its row was
/// removed, so `apply_published_anchors` sees the set it last applied and converges the kept
/// bindings on what its author published since. Returns whether one was restored; a memory never
/// parked is left as it is. The `origin='synced'` gate leaves a local row of the same id — the
/// ghost the removal itself steps around — without a baseline that was never its own; its parked
/// row waits for the repo purge. A rebind made here between the memory's return and the pass that
/// carries its anchors has its hash overwritten by the parked one for that pass alone: the set the
/// rebind published is this account's own, and its next fold stamps the hash back.
fn restore_parked_anchor_baseline(
    tx: &Transaction<'_>,
    repo_id: &str,
    memory_id: &str,
) -> anyhow::Result<bool> {
    const PARKED: &str = "FROM repo_memory_parked_baselines p
         WHERE p.memory_id = repo_memories.id AND p.repo_id = repo_memories.repo_id";
    let restored = tx.execute(
        &format!(
            "UPDATE repo_memories
                SET anchors_applied_digest = (SELECT p.anchors_applied_digest {PARKED}),
                    anchors_applied_targets = (SELECT p.anchors_applied_targets {PARKED}),
                    source_text_hash = (SELECT p.source_text_hash {PARKED})
              WHERE id = ?1 AND repo_id = ?2 AND origin = 'synced' AND EXISTS (SELECT 1 {PARKED})"
        ),
        params![memory_id, repo_id],
    )?;
    if restored > 0 {
        tx.execute(
            "DELETE FROM repo_memory_parked_baselines WHERE memory_id = ?1 AND repo_id = ?2",
            params![memory_id, repo_id],
        )?;
    }
    Ok(restored > 0)
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
#[path = "drain_tests.rs"]
mod tests;
