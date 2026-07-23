//! The accepted-`/3` → memory projection fold (sync phase C3.4b-i, #663).
//!
//! The `/3` acceptance layer classifies signed envelopes; it never decodes op payloads, so it has
//! no analog of the `/1` shadow projection ([`crate::store::reproject`]) — no anti-join, no
//! ghost detection. This module is that missing fold: for one `/2` stream it loads the ACCEPTED
//! `/3` entries (`content_entries WHERE accepted = 1`), [`op::decode`]s each body, folds them
//! through the shared memory projector ([`project::project`]), and materializes the result into
//! `content_projected_nodes` / `content_projected_edges` (V070), keyed by the `/2` stream. It is a
//! MEMORY-layer concern (it decodes op bodies) reading the acceptance-gated set, so it lives here
//! in the op-log memory layer alongside `store`/`project`, not inside the body-agnostic
//! `account::content`.
//!
//! SEPARATE TABLES (decision 7). The `/3` projection is NOT written to the `/1` shadow tables. The
//! `/1` projector sweep ([`crate::store::reproject`]'s `reproject_all_streams`) `DELETE`s
//! the `oplog_projected_*` tables wholesale and rebuilds only streams present in `oplog_entries`,
//! so a projector-version bump would wipe a shared `/3` projection and never rebuild it — mass
//! duplicate re-authoring into the immutable `/3` log. These tables are owned by the memory layer
//! and updated only when acceptance changes (the content refold — the local author seam, and later
//! the ingest / account→content retro-triggers), never by the `/1` sweep.
//!
//! The projected `content_json` / `spec_json` / `resolved_json` reuse the `/1` shadow-row DTOs
//! ([`crate::store::NodeContentRow`] et al.), so the two projections serialize identically.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::account::{
    KeyId, content_projected_tables_exist, decode_content_signed, historical_content_keyring,
    open_sealed_payload, stream_owner_account,
};
use super::identity::load_local_device;
use super::op::{
    self, DecodedOp, EdgeSpec, Entry, NodeContent, NodeStatus, OpMeta, ResolvedAnchor,
};
use super::project;
use super::project::ProjectedState;
use super::store::{EdgeSpecRow, NodeContentRow, ResolvedAnchorRow};
use super::stream::StreamId;

/// Bump when the accepted-`/3` → memory fold's projectable set or LWW semantics change (a new op
/// kind becomes `Known`, a register is added). A `/3` projection stamped with an older version is
/// rebuilt WHOLESALE on the next store open — [`rebuild_all_content_projections_if_stale`] runs at
/// the open/migrate seam and re-folds every stream before stamping — never trusted incrementally;
/// a NEWER stamp blocks this binary from reprojecting at all (see
/// [`assert_content_projector_not_newer`]).
// v3 (#691 A-pre): `content_projected_edges` now retains edge TOMBSTONES (present=0) instead of
// dropping removed edges. The bump forces a rebuild so existing stores materialize tombstones for
// already-removed edges (else a foreign EdgeRemove folded before the upgrade would never
// tombstone).
const CONTENT_PROJECTOR_VERSION: i64 = 3;

/// The `oplog_meta` key holding the `/3` projector version the content projection was last folded
/// by. DISTINCT from the `/1` `projector_version` (they evolve independently and share one meta
/// table): a `/1` projector bump must not silently invalidate the `/3` projection, or vice versa.
const CONTENT_PROJECTOR_VERSION_KEY: &str = "content_projector_version";

/// Re-derive the accepted-`/3` → memory projection for one `/2` stream from the CURRENT accepted
/// set and rewrite its rows in both `/3` projection tables — another stream's rows are never
/// touched. Runs inside the caller's txn; called right after `refold_content_stream` (acceptance
/// changed), so the projection always reflects the just-committed accepted DAG.
///
/// VERSION DISCIPLINE (#688), mirroring the `/1` two-part design
/// ([`crate::store::reproject_if_projector_stale`] + the per-write stamp): this per-stream path
/// only ever MAINTAINS an already-current store-global stamp. The store-global version is upgraded
/// EXCLUSIVELY by [`rebuild_all_content_projections_if_stale`], which rebuilds EVERY stream before
/// the one stamp — so the stamp can never come to cover another stream's stale projection (which
/// the memory reconcile's anti-join would then trust, mass-duplicating or skipping rows against
/// it). A missing or older stamp is therefore left UNTOUCHED here: the open-path trigger stays
/// owed its rebuild-all, and this stream's just-written rows are simply rebuilt again there (the
/// fold is idempotent).
pub fn reproject_accepted_content_stream(
    tx: &Transaction<'_>,
    stream_id: StreamId,
) -> anyhow::Result<()> {
    // Refuse to reproject if a NEWER binary already owns this store's `/3` projection: the
    // wholesale DELETE + rebuild below would drop ops the newer binary decodes as `Known` (this
    // binary reads them as `Unknown` and skips them), leaving those nodes ABSENT from
    // `content_projected_nodes`. The memory reconcile's anti-join would then read them as
    // unauthored and mass-duplicate them into the immutable `/3` log. Guard BEFORE any write.
    assert_content_projector_not_newer(tx)?;
    reproject_stream_projection(tx, stream_id)?;
    // Stamp only when the store is already current (see the fn doc): this maintains the stamp, it
    // never upgrades it.
    if stored_content_projector_version(tx)? == Some(CONTENT_PROJECTOR_VERSION) {
        stamp_content_projector_version(tx)?;
    }
    Ok(())
}

/// The store-global upgrade re-fold for the `/3` projection (#688) — the `/3` analog of the `/1`
/// [`crate::store::reproject_if_projector_stale`], wired into the index open/migrate seam (after
/// migrations, so a store that just gained V070 has its tables). When the stored content-projector
/// stamp is MISSING or STRICTLY OLDER than this binary's projector, re-fold EVERY `/2` stream
/// holding accepted content, then stamp the store-global version ONCE; a current or NEWER stamp is
/// left intact (never downgraded, never re-folded). Returns whether it rebuilt.
///
/// This is the ONLY path allowed to raise the stored version: running before any per-stream write,
/// it makes the store current so [`reproject_accepted_content_stream`]'s stamp only ever maintains
/// an already-current store. The rebuild is idempotent and serialized by its own `IMMEDIATE` txn,
/// so racing openers converge without an extra lock (the loser either observes the winner's
/// current stamp and no-ops, or rebuilds the same state again).
pub fn rebuild_all_content_projections_if_stale(conn: &Connection) -> anyhow::Result<bool> {
    // Guard FIRST on the V070 `content_projected_*` tables existing: on a pre-V070 store
    // mid-migration there is no projection to rebuild (and nothing to stamp against). Reuses the
    // #683 guard the refold/settle reprojects carry. It also runs before the `oplog_meta` read,
    // so a bare mid-migration DB missing both never errors here.
    if !content_projected_tables_exist(conn)? {
        return Ok(false);
    }
    match stored_content_projector_version(conn)? {
        Some(version) if version >= CONTENT_PROJECTOR_VERSION => Ok(false),
        _ => {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            // The stale check above already excludes a newer stamp; assert again inside the write
            // txn so this fn stays honest if the arm above ever changes (mirrors
            // [`crate::store::rebuild_projection`]).
            assert_content_projector_not_newer(&tx)?;
            reproject_all_accepted_content_streams(&tx)?;
            stamp_content_projector_version(&tx)?;
            tx.commit()?;
            Ok(true)
        },
    }
}

/// Re-fold EVERY `/2` stream holding accepted `/3` content wholesale: clear BOTH projection tables
/// first so a row whose stream's accepted set emptied since its last reproject (a full
/// retro-condemn) cannot linger, then fold each stream (mirrors [`crate::store`]'s
/// `reproject_all_streams`). Runs inside the caller's txn; the caller stamps after.
fn reproject_all_accepted_content_streams(tx: &Transaction<'_>) -> anyhow::Result<()> {
    tx.execute("DELETE FROM content_projected_nodes", [])?;
    tx.execute("DELETE FROM content_projected_edges", [])?;
    for stream_id in accepted_content_streams(tx)? {
        reproject_stream_projection(tx, stream_id)?;
    }
    Ok(())
}

/// Every distinct `/2` stream the log currently holds ACCEPTED `/3` content for — the rebuild set
/// for a projector upgrade (mirrors [`crate::store`]'s `streams_present`).
fn accepted_content_streams(conn: &Connection) -> anyhow::Result<Vec<StreamId>> {
    let mut stmt =
        conn.prepare("SELECT DISTINCT stream_id FROM content_entries WHERE accepted = 1")?;
    let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut streams = Vec::new();
    for row in rows {
        let bytes = row?;
        let hash: [u8; 32] =
            bytes.try_into().map_err(|_| anyhow::anyhow!("stored /3 stream_id is not 32 bytes"))?;
        streams.push(StreamId::from_bytes(hash));
    }
    Ok(streams)
}

/// The full-replay fold for ONE stream: load its accepted entries, `project`, and rewrite its rows
/// in BOTH `/3` projection tables — another stream's projection is never touched. Carries NO
/// version logic; the callers own the stamp discipline.
fn reproject_stream_projection(tx: &Transaction<'_>, stream_id: StreamId) -> anyhow::Result<()> {
    let entries = load_accepted_entries(tx, stream_id)?;
    let state = project::project(&entries);
    write_projection(tx, stream_id, &state)
}

/// Error if a NEWER `/3` projector already folded this store's content projection — an older binary
/// must not reproject (it would drop ops the newer binary knows) or stamp the version down. Mirrors
/// the `/1` guard in [`crate::store`], at the `/3` projection layer (a projector bump need
/// not carry a schema bump, so the schema guard does not cover it). Store-global, not per stream:
/// one binary folds every stream it holds.
fn assert_content_projector_not_newer(conn: &Connection) -> anyhow::Result<()> {
    if let Some(stored) = stored_content_projector_version(conn)?
        && stored > CONTENT_PROJECTOR_VERSION
    {
        anyhow::bail!(
            "the /3 content projection was folded by a newer rag-rat (content projector v{stored} \
             > v{CONTENT_PROJECTOR_VERSION}); upgrade to write this store"
        );
    }
    Ok(())
}

/// Write the store-global `/3` projector stamp. Two callers, two roles (#688):
/// [`rebuild_all_content_projections_if_stale`] UPGRADES it (after rebuilding every stream), and
/// [`reproject_accepted_content_stream`] only MAINTAINS it (called solely when the stored stamp
/// is already current).
fn stamp_content_projector_version(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO oplog_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![CONTENT_PROJECTOR_VERSION_KEY, CONTENT_PROJECTOR_VERSION.to_string()],
    )?;
    Ok(())
}

fn stored_content_projector_version(conn: &Connection) -> anyhow::Result<Option<i64>> {
    conn.query_row(
        "SELECT value FROM oplog_meta WHERE key = ?1",
        params![CONTENT_PROJECTOR_VERSION_KEY],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|value| value.parse::<i64>().context("oplog content_projector_version is not an integer"))
    .transpose()
}

/// Rewrite one stream's rows in `content_projected_nodes` / `content_projected_edges` — clear the
/// stream's prior rows, then insert the folded state (mirrors [`crate::store::reproject`]).
fn write_projection(
    tx: &Transaction<'_>,
    stream_id: StreamId,
    state: &ProjectedState,
) -> anyhow::Result<()> {
    let stream_bytes = stream_id.to_bytes();
    tx.execute("DELETE FROM content_projected_nodes WHERE stream_id = ?1", params![
        stream_bytes.as_slice()
    ])?;
    tx.execute("DELETE FROM content_projected_edges WHERE stream_id = ?1", params![
        stream_bytes.as_slice()
    ])?;
    for (node_id, node) in &state.nodes {
        let content_json = serde_json::to_string(&NodeContentRow::from(&node.content))
            .context("serialize projected /3 node content")?;
        tx.execute(
            "INSERT INTO content_projected_nodes(stream_id, node_id, content_json, status)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                stream_bytes.as_slice(),
                node_id.as_str(),
                content_json,
                node.status.as_db_str()
            ],
        )?;
    }
    // Live edges (present=1) AND tombstones (present=0). The tombstones are RETAINED, not dropped,
    // so a projection consumer honors a foreign `EdgeRemove`: the memory reconcile treats a
    // tombstoned edge as authored (never re-adds it) and the projection deletes the read-table
    // row. Dropping them lets a foreign remove be re-authored at a fresh Lamport — a
    // cross-device growth loop (#691 A-pre).
    for (present, edges) in [(1, &state.edges), (0, &state.removed_edges)] {
        for (edge_key, edge) in edges {
            let spec_json = serde_json::to_string(&EdgeSpecRow::from(&edge.spec))
                .context("serialize projected /3 edge spec")?;
            let resolved_json = edge
                .resolved
                .as_ref()
                .map(|resolved| serde_json::to_string(&ResolvedAnchorRow::from(resolved)))
                .transpose()
                .context("serialize projected /3 edge resolved anchor")?;
            tx.execute(
                "INSERT INTO content_projected_edges(stream_id, edge_key, spec_json, \
                 resolved_json,
                 present) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    stream_bytes.as_slice(),
                    edge_key.as_str(),
                    spec_json,
                    resolved_json,
                    present
                ],
            )?;
        }
    }
    Ok(())
}

/// Load one stream's ACCEPTED `/3` entries as projector [`Entry`]s: decode the content envelope for
/// its `lamport` + `device_fingerprint` (the `(lamport, device)` LWW order), then [`op::decode`]
/// the body. An `Unknown` op is retained in the log but skipped here (mirrors
/// [`crate::store`]'s `load_known_entries`), so a forward-version op never breaks the fold.
fn load_accepted_entries(tx: &Transaction<'_>, stream_id: StreamId) -> anyhow::Result<Vec<Entry>> {
    // Key wraps live in the immutable stream OWNER's secrets log. A granted writer's account is
    // only the content author and may have no copy of those wraps.
    let owner_account = stream_owner_account(tx, stream_id)?;
    // Projection reads never mint or backfill a local identity. A keyless peer still retains and
    // accepts suite-1 entries; it simply cannot project them locally yet.
    let device = load_local_device(tx)?;
    let keyring = match (owner_account, device.as_ref()) {
        (Some(owner), Some(device)) =>
            Some(historical_content_keyring(tx, owner, stream_id, device)?),
        _ => None,
    };
    let mut stmt = tx.prepare(
        "SELECT entry_hash, stream_id, signed_bytes FROM content_entries
         WHERE stream_id = ?1 AND accepted = 1
         ORDER BY entry_hash", /* deterministic load order (the projector sorts internally
                                * regardless) */
    )?;
    let rows = stmt.query_map(params![stream_id.to_bytes().as_slice()], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (stored_entry_hash, stored_stream_id, signed_bytes) = row?;
        let stored_entry_hash: [u8; 32] = stored_entry_hash
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored accepted /3 entry_hash is not 32 bytes"))?;
        let stored_stream_id: [u8; 32] = stored_stream_id
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored accepted /3 stream_id is not 32 bytes"))?;
        // The signed ENVELOPE decoded at ingest to become a candidate, so a failure here is
        // corruption at rest — surface it loudly.
        let signed = decode_content_signed(&signed_bytes)
            .context("stored accepted /3 entry failed to decode")?;
        anyhow::ensure!(
            signed.entry_hash == stored_entry_hash,
            "stored accepted /3 signed envelope does not match its entry_hash row"
        );
        anyhow::ensure!(
            signed.header.stream_id.to_bytes() == stored_stream_id
                && signed.header.stream_id == stream_id,
            "stored accepted /3 signed envelope does not match its stream_id row"
        );
        // Payload/key failures are LOCAL projection failures, not acceptance failures. Unknown
        // suites, absent exact keys, malformed sealed payloads, tag failures, and undecodable
        // plaintext all remain retained+accepted and skip only this entry.
        let plaintext;
        let op_bytes = match signed.header.crypto_suite {
            0 => signed.payload.as_slice(),
            1 => {
                let Some(key_id) = signed.header.key_id else {
                    continue;
                };
                let Some(key) =
                    keyring.as_ref().and_then(|keyring| keyring.get(KeyId::from_bytes(key_id)))
                else {
                    continue;
                };
                let Ok(opened) = open_sealed_payload(key, &signed.payload, &signed.header_bytes)
                else {
                    continue;
                };
                plaintext = opened;
                plaintext.as_slice()
            },
            _ => continue,
        };
        let Ok(decoded) = op::decode(op_bytes) else {
            continue;
        };
        match decoded {
            DecodedOp::Known(op) => entries.push(Entry {
                meta: OpMeta {
                    lamport: signed.header.lamport,
                    device: signed.header.device_fingerprint,
                },
                op,
            }),
            DecodedOp::Unknown { .. } => {}, // retained in the log, not projected
        }
    }
    Ok(entries)
}

/// One projected `/3` node, decoded for a projection consumer: the stable node id, the folded
/// content register, and the folded status. The memory drain (rag-rat-core) reads these to mirror
/// a stream's accepted nodes into `repo_memories` as `origin='synced'` rows — the reverse direction
/// of the local reconcile. Owned + flat: the private row DTOs stay inside this crate.
pub struct ProjectedContentNode {
    pub node_id: String,
    pub content: NodeContent,
    pub status: NodeStatus,
}

/// One projected `/3` edge, decoded for a projection consumer: the stable key, the folded spec (the
/// edge is self-describing — it carries its own `owner_repo_id`), the last resolved anchor if any,
/// and `present` (a `false` row is a RETAINED tombstone the consumer honors, not a live edge).
pub struct ProjectedContentEdge {
    pub edge_key: String,
    pub spec: EdgeSpec,
    pub resolved: Option<ResolvedAnchor>,
    pub present: bool,
}

/// Read one `/2` stream's projected nodes, decoded from the stored `content_json` back into the op
/// model. Deterministic `node_id` order. An unknown status token FAILS (a newer binary folded it),
/// mirroring the reconcile's authoring guard rather than silently coercing it.
pub fn list_projected_content_nodes(
    conn: &Connection,
    stream_id: StreamId,
) -> anyhow::Result<Vec<ProjectedContentNode>> {
    let mut stmt = conn.prepare(
        "SELECT node_id, content_json, status FROM content_projected_nodes
         WHERE stream_id = ?1 ORDER BY node_id",
    )?;
    let rows = stmt.query_map(params![stream_id.to_bytes().as_slice()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    })?;
    let mut nodes = Vec::new();
    for row in rows {
        let (node_id, content_json, status) = row?;
        let content: NodeContentRow = serde_json::from_str(&content_json)
            .with_context(|| format!("decode projected /3 node content for `{node_id}`"))?;
        let status = NodeStatus::from_db_str(&status).with_context(|| {
            format!("projected /3 node `{node_id}` carries unknown status `{status}`")
        })?;
        nodes.push(ProjectedContentNode { node_id, content: NodeContent::from(content), status });
    }
    Ok(nodes)
}

/// Read one `/2` stream's projected edges (live AND tombstoned), decoded from the stored
/// `spec_json` / `resolved_json` back into the op model. Deterministic `edge_key` order.
pub fn list_projected_content_edges(
    conn: &Connection,
    stream_id: StreamId,
) -> anyhow::Result<Vec<ProjectedContentEdge>> {
    let mut stmt = conn.prepare(
        "SELECT edge_key, spec_json, resolved_json, present FROM content_projected_edges
         WHERE stream_id = ?1 ORDER BY edge_key",
    )?;
    let rows = stmt.query_map(params![stream_id.to_bytes().as_slice()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut edges = Vec::new();
    for row in rows {
        let (edge_key, spec_json, resolved_json, present) = row?;
        let spec_row: EdgeSpecRow = serde_json::from_str(&spec_json)
            .with_context(|| format!("decode projected /3 edge spec for `{edge_key}`"))?;
        let resolved = resolved_json
            .map(|json| {
                serde_json::from_str::<ResolvedAnchorRow>(&json)
                    .with_context(|| format!("decode projected /3 edge anchor for `{edge_key}`"))
                    .map(ResolvedAnchor::from)
            })
            .transpose()?;
        edges.push(ProjectedContentEdge {
            edge_key,
            spec: EdgeSpec::try_from(spec_row)?,
            resolved,
            present: present != 0,
        });
    }
    Ok(edges)
}
