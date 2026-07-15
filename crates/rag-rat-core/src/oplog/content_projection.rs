//! The accepted-`/3` → memory projection fold (sync phase C3.4b-i, #663).
//!
//! The `/3` acceptance layer classifies signed envelopes; it never decodes op payloads, so it has
//! no analog of the `/1` shadow projection ([`crate::oplog::store::reproject`]) — no anti-join, no
//! ghost detection. This module is that missing fold: for one `/2` stream it loads the ACCEPTED
//! `/3` entries (`content_entries WHERE accepted = 1`), [`op::decode`]s each body, folds them
//! through the shared memory projector ([`project::project`]), and materializes the result into
//! `content_projected_nodes` / `content_projected_edges` (V070), keyed by the `/2` stream. It is a
//! MEMORY-layer concern (it decodes op bodies) reading the acceptance-gated set, so it lives here
//! in the op-log memory layer alongside `store`/`project`, not inside the body-agnostic
//! `account::content`.
//!
//! SEPARATE TABLES (decision 7). The `/3` projection is NOT written to the `/1` shadow tables. The
//! `/1` projector sweep ([`crate::oplog::store::reproject`]'s `reproject_all_streams`) `DELETE`s
//! the `oplog_projected_*` tables wholesale and rebuilds only streams present in `oplog_entries`,
//! so a projector-version bump would wipe a shared `/3` projection and never rebuild it — mass
//! duplicate re-authoring into the immutable `/3` log. These tables are owned by the memory layer
//! and updated only when acceptance changes (the content refold — the local author seam, and later
//! the ingest / account→content retro-triggers), never by the `/1` sweep.
//!
//! The projected `content_json` / `spec_json` / `resolved_json` reuse the `/1` shadow-row DTOs
//! ([`crate::oplog::store::NodeContentRow`] et al.), so the two projections serialize identically.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::account::decode_content_signed;
use super::op::{self, DecodedOp, Entry, OpMeta};
use super::project;
use super::project::ProjectedState;
use super::store::{EdgeSpecRow, NodeContentRow, ResolvedAnchorRow};
use super::stream::StreamId;

/// Bump when the accepted-`/3` → memory fold's projectable set or LWW semantics change (a new op
/// kind becomes `Known`, a register is added). A `/3` projection stamped with an older version is
/// rebuilt on the next content refold, never trusted incrementally; a NEWER stamp blocks this
/// binary from reprojecting at all (see [`assert_content_projector_not_newer`]).
const CONTENT_PROJECTOR_VERSION: i64 = 1;

/// The `oplog_meta` key holding the `/3` projector version the content projection was last folded
/// by. DISTINCT from the `/1` `projector_version` (they evolve independently and share one meta
/// table): a `/1` projector bump must not silently invalidate the `/3` projection, or vice versa.
const CONTENT_PROJECTOR_VERSION_KEY: &str = "content_projector_version";

/// Re-derive the accepted-`/3` → memory projection for one `/2` stream from the CURRENT accepted
/// set and rewrite its rows in both `/3` projection tables — another stream's rows are never
/// touched. Runs inside the caller's txn; called right after `refold_content_stream` (acceptance
/// changed), so the projection always reflects the just-committed accepted DAG.
pub(in crate::oplog) fn reproject_accepted_content_stream(
    tx: &Transaction<'_>,
    stream_id: StreamId,
) -> anyhow::Result<()> {
    // Refuse to reproject if a NEWER binary already owns this store's `/3` projection: the
    // wholesale DELETE + rebuild below would drop ops the newer binary decodes as `Known` (this
    // binary reads them as `Unknown` and skips them), leaving those nodes ABSENT from
    // `content_projected_nodes`. The memory reconcile's anti-join would then read them as
    // unauthored and mass-duplicate them into the immutable `/3` log. Guard BEFORE any write.
    assert_content_projector_not_newer(tx)?;
    let entries = load_accepted_entries(tx, stream_id)?;
    let state = project::project(&entries);
    write_projection(tx, stream_id, &state)?;
    stamp_content_projector_version(tx)?;
    Ok(())
}

/// Error if a NEWER `/3` projector already folded this store's content projection — an older binary
/// must not reproject (it would drop ops the newer binary knows) or stamp the version down. Mirrors
/// the `/1` guard in [`crate::oplog::store`], at the `/3` projection layer (a projector bump need
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
/// stream's prior rows, then insert the folded state (mirrors [`crate::oplog::store::reproject`]).
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
    for (edge_key, edge) in &state.edges {
        let spec_json = serde_json::to_string(&EdgeSpecRow::from(&edge.spec))
            .context("serialize projected /3 edge spec")?;
        let resolved_json = edge
            .resolved
            .as_ref()
            .map(|resolved| serde_json::to_string(&ResolvedAnchorRow::from(resolved)))
            .transpose()
            .context("serialize projected /3 edge resolved anchor")?;
        tx.execute(
            "INSERT INTO content_projected_edges(stream_id, edge_key, spec_json, resolved_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![stream_bytes.as_slice(), edge_key.as_str(), spec_json, resolved_json],
        )?;
    }
    Ok(())
}

/// Load one stream's ACCEPTED `/3` entries as projector [`Entry`]s: decode the content envelope for
/// its `lamport` + `device_fingerprint` (the `(lamport, device)` LWW order), then [`op::decode`]
/// the body. An `Unknown` op is retained in the log but skipped here (mirrors
/// [`crate::oplog::store`]'s `load_known_entries`), so a forward-version op never breaks the fold.
fn load_accepted_entries(tx: &Transaction<'_>, stream_id: StreamId) -> anyhow::Result<Vec<Entry>> {
    let mut stmt = tx.prepare(
        "SELECT signed_bytes FROM content_entries
         WHERE stream_id = ?1 AND accepted = 1
         ORDER BY entry_hash", /* deterministic load order (the projector sorts internally
                                * regardless) */
    )?;
    let rows =
        stmt.query_map(params![stream_id.to_bytes().as_slice()], |row| row.get::<_, Vec<u8>>(0))?;
    let mut entries = Vec::new();
    for row in rows {
        let signed_bytes = row?;
        // The signed ENVELOPE decoded at ingest to become a candidate, so a failure here is
        // corruption at rest — surface it loudly.
        let signed = decode_content_signed(&signed_bytes)
            .context("stored accepted /3 entry failed to decode")?;
        // The BODY is an opaque bstr the acceptance layer never decodes (§8, body-agnostic), so an
        // authorized+accepted entry can carry a malformed or wrong-domain payload (a foreign entry
        // that `content_ingest` accepted without a body check). Skip an undecodable/unknown body
        // like the `/1` fold skips a non-projectable op — never `bail!`, which would crash every
        // later local author on this stream over one bad row.
        let Ok(decoded) = op::decode(&signed.payload) else {
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
