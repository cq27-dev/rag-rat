//! Translating persisted memories into signed op-log entries and authoring them live, inside each
//! mutation's own transaction; [`reconcile`](super::reconcile) keeps the log a COMPLETE signed
//! mirror of `repo_memories` / `repo_node_edges` (#524, #541, #664).
//!
//! This bridges `repo_memories` / `repo_node_edges` (owned by this module) and the op-log MINTING
//! primitives ([`crate::oplog`]) — a ONE-WAY dependency, so `oplog` never depends back on the
//! memory subsystem (a reverse call would cycle the build).
//!
//! OWNER-BOUND `/2`//3 substrate (#664). The live path authors owner-bound `/3` content on the
//! repo's owner-bound `/2` stream, under the store's single local account (minted once, store-
//! global). Each reconcile/mutation ensures the repo's `/2` stream is owned (publishing a
//! `StreamOwn` account op) and authors its ops as owner-authored `/3` content
//! ([`rag_rat_oplog::author_prepared_content_batch_in_tx`]), which verify-accepts and reprojects
//! into `content_projected_nodes` / `content_projected_edges`. The completeness predicate is an
//! anti-join against that accepted-`/3` projection; the pre-existing `/1` history is retained but
//! no longer written by the live path (existing `/1` rows are adopted into `/3` by the reconcile).
//!
//! WIRED into the live write path (#532): the memory mutations call
//! [`backfill_memory_oplog`](super::reconcile::backfill_memory_oplog) once (before the first live
//! entry) and the `author_*` seams below INSIDE their own transaction, so the op-append and the
//! table write commit — or roll back — together (strict-atomic). Authoring is a NO-OP under an
//! unstable scope ([`stable_owner_stream`](super::reconcile::stable_owner_stream)) or before the
//! local account is minted, leaving scope-less callers untouched.

use rag_rat_oplog::{
    EdgeKey, EdgeSpec, MemoryOp, NodeId, NodeStatus, PreparedContentAuthoring, StreamId,
};
use rag_rat_query::memory::{RepoMemory, memory_repo_scope};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::ownership::{StreamSealPolicy, grantee_context, stream_seal_policy};
use super::reconcile::{
    content_op_is_authorable, node_content_of_memory, stable_owner_stream,
    stable_owner_stream_for_repo,
};

/// Scoped durability bump for an AUTHORED write (#560). The index connection runs
/// `synchronous = NORMAL` — the right policy for the high-frequency, fully reconstructable
/// derived-index writes, where skipping the per-commit WAL fsync is a throughput win and the only
/// cost is that the last committed transaction can roll back on power loss (a re-index recovers
/// it).
///
/// Authored memory / op-log mutations are the OPPOSITE class: irreplaceable, low-frequency, and
/// they return success to the caller. They must not acknowledge under a mode that can silently lose
/// the last commit, so they raise `synchronous = FULL` (fsync the WAL on commit) for the duration
/// of their transaction and restore `NORMAL` on drop. The guard is held ACROSS the authored
/// `BEGIN .. COMMIT` and dropped after, so the commit fsyncs; restore runs on every path (including
/// error/panic), so a shared connection is never stranded at FULL — and a stray failure could only
/// leave it on the *safer*, slower setting, never a less durable one.
pub(super) struct AuthoredDurability<'a> {
    conn: &'a Connection,
}

impl<'a> AuthoredDurability<'a> {
    /// Raise `synchronous = FULL`. MUST be called OUTSIDE a transaction (SQLite only applies a
    /// `synchronous` change to subsequent transactions), i.e. immediately before the authored
    /// `BEGIN`/`unchecked_transaction`.
    pub(super) fn begin(conn: &'a Connection) -> anyhow::Result<Self> {
        conn.execute_batch("PRAGMA synchronous = FULL;")?;
        Ok(Self { conn })
    }
}

impl Drop for AuthoredDurability<'_> {
    fn drop(&mut self) {
        // Best-effort restore of the connection default (see the struct doc for why swallowing is
        // safe). Runs after the authored txn has committed/rolled back, so no transaction is open.
        let _ = self.conn.execute_batch("PRAGMA synchronous = NORMAL;");
    }
}

/// The preamble every AUTHORED memory / edge mutation opens with, in the one order that keeps the
/// authored commit durable. The table writes and the op-append then run in `tx`, so they commit —
/// or roll back — together (strict-atomic); writes through the bare `conn` participate in it.
///
/// 1. Backfill the pre-existing history (idempotent; a cheap no-op once the chain exists). This is
///    what mints the local account and establishes the owner stream: without it a store whose
///    account has never been minted prepares `None`, and `author_in_owner_stream` returns Ok having
///    written nothing.
/// 2. Prepare the live authoring BEFORE the transaction: minting the account self-transacts and
///    cannot nest inside the one opened below.
/// 3. Raise [`AuthoredDurability`] only after both (#560). Each of them may self-transact under its
///    OWN guard, whose drop restores `synchronous = NORMAL` — a guard raised earlier would be
///    downgraded before our commit, silently losing the durability it exists to provide.
/// 4. `BEGIN IMMEDIATE`, not deferred: memory writes are the sanctioned flock-less writers on the
///    shared database, racing foreign repos' rebuilds by design. A deferred txn that READS first
///    and then upgrades to write fails with SQLITE_BUSY_SNAPSHOT the moment a concurrent writer
///    committed in between — and that error BYPASSES the busy handler, so busy_timeout never gets a
///    say. Taking the write lock up front waits it out instead (#818).
///
/// Fields drop in declaration order: an uncommitted `tx` rolls back before the guard restores
/// NORMAL, and [`commit`](Self::commit) commits before the guard drops.
pub(super) struct AuthoredWrite<'a> {
    pub(super) prepared: Option<PreparedOwnerAuthoring>,
    pub(super) tx: Transaction<'a>,
    _durability: AuthoredDurability<'a>,
}

impl<'a> AuthoredWrite<'a> {
    pub(super) fn begin(conn: &'a Connection, now_ms: i64) -> anyhow::Result<Self> {
        super::reconcile::backfill_memory_oplog(conn, now_ms)?;
        let prepared = prepare_live_content_authoring(conn, now_ms)?;
        let durability = AuthoredDurability::begin(conn)?;
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        Ok(Self { prepared, tx, _durability: durability })
    }

    pub(super) fn commit(self) -> anyhow::Result<()> {
        self.tx.commit()?;
        Ok(())
    }
}

pub(crate) struct PreparedOwnerAuthoring {
    repo_id: String,
    /// The `/2` stream to author onto — this store's own owned stream in `Owner` mode, the
    /// CONFIGURED owner's stream in `Grantee` mode.
    stream: StreamId,
    role: AuthoringRole,
}

/// How this store authors `/3` content for a repo. `Owner` is the default — the local account owns
/// the stream. `Grantee` (#1164) is a granted contributor authoring onto ANOTHER account's stream.
enum AuthoringRole {
    Owner { policy: StreamSealPolicy, prepared: PreparedContentAuthoring },
    Grantee { owner_account: rag_rat_oplog::AccountId, grant_id: [u8; 32] },
}

impl PreparedOwnerAuthoring {
    /// The owner-mode prepared `/3` batch. The reconcile paths build and use Owner-role handles
    /// only (a contributor skips reconcile via `backfill_memory_oplog`), so a Grantee handle
    /// here is a programming error, not a runtime condition.
    pub(super) fn owner_prepared(&self) -> anyhow::Result<&PreparedContentAuthoring> {
        match &self.role {
            AuthoringRole::Owner { prepared, .. } => Ok(prepared),
            AuthoringRole::Grantee { .. } => anyhow::bail!(
                "reconcile is owner-only, but a grantee-role prepared handle reached it"
            ),
        }
    }
}

// A granted contributor's rows stay `origin='local'`, which is their AUTHORSHIP: this store wrote
// them, and `origin` is what `ImportMode::SeedPublic` reads to decide whose memories a public seed
// carries. It is deliberately NOT re-purposed to mean "under the drain's removal authority" — those
// two meanings diverge for exactly these rows (locally authored, yet projected on another account's
// stream), and one column cannot carry both.
//
// The consequence is bounded and accepted: when an authority refold condemns a contribution (a
// revoked grant, a device cut ordered before it), the owner's stream stops accepting and serving
// it, but the contributor keeps its own copy of its own writing. Content this store RECEIVED is
// `origin='synced'` and the drain's anti-join does remove it, so a revoke never leaves another
// account's condemned content readable here. Separating the two would need a third `origin` value
// (a CHECK rewrite on both tables) — worth it only once a case appears where a contributor must
// forget what it authored itself.

pub(super) fn prepare_owner_authoring(
    conn: &Connection,
    repo_id: &str,
    stream: StreamId,
    policy: StreamSealPolicy,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<Option<PreparedOwnerAuthoring>> {
    if ops.is_empty() {
        return Ok(None);
    }
    for op in ops {
        reject_unauthorable_content_op(op, policy)?;
    }
    let prepared =
        rag_rat_oplog::prepare_content_authoring(conn, stream, policy.seal_policy(), now_ms)?;
    Ok(Some(PreparedOwnerAuthoring {
        repo_id: repo_id.to_string(),
        stream,
        role: AuthoringRole::Owner { policy, prepared },
    }))
}

pub(crate) fn prepare_live_authoring(
    conn: &Connection,
    ops: &[MemoryOp],
    now_ms: i64,
) -> anyhow::Result<Option<PreparedOwnerAuthoring>> {
    if ops.is_empty() {
        return Ok(None);
    }
    let Some(repo_id) = memory_repo_scope(conn)? else {
        for op in ops {
            reject_unauthorable_content_op(op, StreamSealPolicy::Plaintext)?;
        }
        return Ok(None);
    };
    // Grantee mode (#1164): a granted contributor authors onto the CONFIGURED owner's stream via
    // its grant. Grants target public plaintext streams, so the authorability guard uses
    // Plaintext.
    if let Some(ctx) = grantee_context(conn, &repo_id)? {
        for op in ops {
            reject_unauthorable_content_op(op, StreamSealPolicy::Plaintext)?;
        }
        return Ok(Some(PreparedOwnerAuthoring {
            repo_id,
            stream: ctx.stream,
            role: AuthoringRole::Grantee {
                owner_account: ctx.owner_account,
                grant_id: ctx.grant_id,
            },
        }));
    }
    let Some(stream) = stable_owner_stream_for_repo(conn, &repo_id)? else {
        for op in ops {
            reject_unauthorable_content_op(op, StreamSealPolicy::Plaintext)?;
        }
        return Ok(None);
    };
    let policy = stream_seal_policy(conn, &repo_id, stream)?;
    prepare_owner_authoring(conn, &repo_id, stream, policy, ops, now_ms)
}

pub(crate) fn prepare_live_content_authoring(
    conn: &Connection,
    now_ms: i64,
) -> anyhow::Result<Option<PreparedOwnerAuthoring>> {
    let sentinel = MemoryOp::EdgeRemove { edge_key: EdgeKey::from("live-authoring-preparation") };
    prepare_live_authoring(conn, &[sentinel], now_ms)
}

/// Reject a live content op the `/3` log cannot carry — either a shape `op::decode` would refuse
/// at any size, or an ASSEMBLED signed envelope over the §18a 256 KiB cap (#680). The
/// AUTHORITATIVE whole-op write-boundary guard. The cheap per-field caps
/// (`validate_payload`'s payload byte cap, `validate_edge_len`'s edge-anchor cap, the title/body
/// char caps) fast-fail a single pathological field, but they cannot see an AGGREGATE — most
/// reachably an arbitrary NUMBER of individually-valid tags on a `NodeCreate`/`NodeUpdate`, or a
/// max-ish payload + a long body + many tags TOGETHER — nor a FUTURE uncapped field. This one
/// check, built on the SAME [`rag_rat_oplog::content_op_is_authorable`] the reconcile quarantine
/// uses, rejects every such op before it is signed, so a write the guard accepts is exactly one the
/// reconcile can later author. Without it an "otherwise valid" create/update assembles an
/// un-authorable op that the #680 reconcile quarantine then SILENTLY skips — the user never learns
/// at write time.
fn reject_unauthorable_content_op(op: &MemoryOp, policy: StreamSealPolicy) -> anyhow::Result<()> {
    if content_op_is_authorable(op, policy) {
        return Ok(());
    }
    // Two different failures reach here and they want different remedies, so name the right one.
    // A structural refusal is not a size problem: 65 tiny anchors are nowhere near the byte cap,
    // and a binding named twice is a dedupe fix, not a "shorten it" one.
    if !rag_rat_oplog::within_wire_limits(op) {
        match op {
            MemoryOp::NodeAnchors { node_id, anchors } => anyhow::bail!(
                "memory `{}` cannot store its {} anchors: an anchor set holds at most {} bindings \
                 and must not name one binding twice",
                node_id.as_str(),
                anchors.len(),
                rag_rat_oplog::MAX_ANCHORS_PER_OP
            ),
            MemoryOp::NodeAnchorScopes { node_id, scopes } => anyhow::bail!(
                "memory `{}` cannot store its {} anchor scopes: a scope set holds at most {} \
                 entries and must not name one binding twice",
                node_id.as_str(),
                scopes.len(),
                rag_rat_oplog::MAX_ANCHORS_PER_OP
            ),
            // No other op kind has a structural limit today; this arm keeps the branch total.
            _ => anyhow::bail!("this memory operation has a shape the op log cannot encode"),
        }
    }
    // Past the structural gate the culprit is size: an aggregate the per-field caps cannot see (or
    // a future uncapped field). Name what to shrink rather than surfacing a bare envelope-overflow
    // rollback.
    match op {
        MemoryOp::NodeCreate { node_id, .. } | MemoryOp::NodeUpdate { node_id, .. } =>
            anyhow::bail!(
                "memory `{}` is too large to store: even with each field within its own limit, \
                 its title, body, payload and tags together exceed the 256 KiB signed-entry cap — \
                 reduce the number of tags, or shrink the body/payload",
                node_id.as_str()
            ),
        MemoryOp::EdgeAdd { edge } => anyhow::bail!(
            "the edge from `{}` is too large to store: its assembled fields exceed the 256 KiB \
             signed-entry cap — shorten the target anchor / target repo id",
            edge.source_node_id.as_str()
        ),
        MemoryOp::NodeAnchors { node_id, anchors } => anyhow::bail!(
            "memory `{}` is too large to store: its {} anchors exceed the 256 KiB signed-entry \
             cap — shorten their paths, or bind the memory to fewer places",
            node_id.as_str(),
            anchors.len()
        ),
        // NodeStatus / EdgeRemove / Rebind / NodeSourceHash carry no unbounded free-form field a
        // caller controls — a source hash is a fixed-width digest — so they cannot exceed the cap;
        // this arm keeps the guard total over the op vocabulary.
        _ => anyhow::bail!(
            "this memory operation is too large to store: its assembled /3 content envelope \
             exceeds the 256 KiB signed-entry cap"
        ),
    }
}

/// Author `ops` as owner-authored `/3` content on the active repo's owner-bound `/2` stream WITHIN
/// the caller's mutation txn — the strict-atomic live seam.
/// [`rag_rat_oplog::author_prepared_content_batch_in_tx`] inserts, refolds, and verify-accepts the
/// batch (no open/commit), so an authoring error propagates via `?` and the caller's txn rolls the
/// table write back with it. A NO-OP under an unstable scope or before the local account is minted.
/// The caller MUST have run `backfill_memory_oplog` first, so the store's account plus `StreamOwn`
/// are established (else the batch's verify-accepted rolls back) and the pre-existing history
/// precedes this live entry.
fn author_in_owner_stream(
    tx: &Transaction<'_>,
    ops: &[MemoryOp],
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    // Whole-op write-boundary guard (#680): every live mutation
    // (`create_memory`/`update_memory`/`rebind_memory`/`add_edge`/`remove_edge`) funnels its
    // authored ops through
    // this ONE seam, so rejecting an un-authorable op here — before it is signed — is the single
    // authoritative catch-all for the aggregate no per-field cap sees (e.g. thousands of
    // individually-valid tags) and any future uncapped field. Runs BEFORE the scope gate so an
    // un-authorable op is rejected consistently even on a not-yet-owned stream (the reconcile does
    // NOT pass through here, so its pre-cap/imported-row quarantine is unaffected).
    // Skip a no-op mutation: an empty batch would still refold + reproject the whole stream for no
    // change. (The four live seams only reach here with non-empty ops today, but the guard keeps a
    // change-free `author_update` from doing O(chain) work.)
    if ops.is_empty() {
        return Ok(());
    }
    let Some(prepared) = prepared else {
        // Scope-less and unstable-scope callers intentionally do not author.
        anyhow::ensure!(stable_owner_stream(tx)?.is_none(), "missing prepared owner authoring");
        return Ok(());
    };
    anyhow::ensure!(
        memory_repo_scope(tx)?.as_deref() == Some(prepared.repo_id.as_str()),
        "prepared /3 authoring belongs to a different repo scope"
    );
    // The whole-op authorability guard runs for both roles; a grant targets a public plaintext
    // stream, so its guard policy is Plaintext.
    let guard_policy = match &prepared.role {
        AuthoringRole::Owner { policy, .. } => *policy,
        AuthoringRole::Grantee { .. } => StreamSealPolicy::Plaintext,
    };
    for op in ops {
        reject_unauthorable_content_op(op, guard_policy)?;
    }
    match &prepared.role {
        AuthoringRole::Owner { policy, prepared: content } => {
            anyhow::ensure!(
                stream_seal_policy(tx, &prepared.repo_id, prepared.stream)? == *policy,
                "memory stream seal policy changed while preparing live authoring; retry"
            );
            rag_rat_oplog::author_prepared_content_batch_in_tx(
                tx,
                prepared.stream,
                ops,
                content,
                now_ms,
            )?;
        },
        // Grantee: author onto the OWNER's stream citing the grant (#1164). No seal-policy recheck
        // — the stream is the owner's and v1 grants are plaintext-public.
        AuthoringRole::Grantee { owner_account, grant_id } => {
            rag_rat_oplog::author_grantee_content_batch_in_tx(
                tx,
                prepared.stream,
                *owner_account,
                *grant_id,
                ops,
                now_ms,
            )?;
        },
    }
    Ok(())
}

/// Author a live memory CREATE (`NodeCreate`) inside the caller's mutation txn. A fresh memory has
/// no node-edges yet, so this is a single op.
pub(crate) fn author_create(
    tx: &Transaction<'_>,
    memory: &RepoMemory,
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let node_id = NodeId::from(memory.memory_id.as_str());
    let mut ops = vec![MemoryOp::NodeCreate {
        node_id: node_id.clone(),
        content: node_content_of_memory(memory),
    }];
    ops.extend(anchor_publication_ops(tx, &memory.memory_id)?);
    author_in_owner_stream(tx, &ops, prepared, now_ms)
}

/// Author the memory's CURRENT anchor set, for a caller that just changed which code it points at.
/// A full-set snapshot, so the op says what the bindings are now rather than how they got there.
pub(crate) fn author_anchors(
    tx: &Transaction<'_>,
    memory_id: &str,
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    // A rebind re-stamps `source_text_hash` in the same transaction, so the published hash has to
    // move with the anchors or a peer keeps comparing against the pre-rebind text. A target with no
    // hash publishes an EMPTY one: the register has no other retraction, and a receiver applies the
    // hash on its own change, so silence would pair the new anchors with the old text.
    let ops = anchor_publication_ops(tx, memory_id)?;
    author_in_owner_stream(tx, &ops, prepared, now_ms)
}

/// The ops that publish a memory's anchor set — its source hash, its anchor scopes, then the set —
/// or none when the memory holds no binding. A memory with no hash publishes an EMPTY one, and one
/// whose symbol anchors have no scope publishes an EMPTY scope set: each register's only
/// retraction, and the ops that keep the triple a triple.
///
/// Always ALL THREE, never one alone. The fold takes a winning set's hash and scopes from the
/// device that wrote it — that device's latest of each — so a device's triple holds against any
/// other writer, whichever order the other wrote its own in. A set published alone would pair with
/// the device's PREVIOUS hash and scopes, the ones describing the target it just left, for good,
/// since nothing republishes afterwards.
///
/// The set goes LAST. The three are separate entries on one chain, and a peer accepts a chain in
/// order, so a pull that stops between them leaves a prefix. Set-last makes that prefix newer
/// companions beside the older set, which the next entry resolves. Set-first would pair the new
/// bindings with the previous target's hash and scopes, and a receiver could read the new set as
/// a retarget away from the target its author just moved to.
pub(crate) fn anchor_publication_ops(
    conn: &Connection,
    memory_id: &str,
) -> anyhow::Result<Vec<MemoryOp>> {
    let Some(anchors) = anchors_op(conn, memory_id)? else {
        return Ok(Vec::new());
    };
    let hash = source_hash_op(conn, memory_id)?.unwrap_or_else(|| MemoryOp::NodeSourceHash {
        node_id: NodeId::from(memory_id),
        source_text_hash: String::new(),
    });
    let scopes = anchor_scopes_op(conn, memory_id)?;
    Ok(vec![hash, scopes, anchors])
}

/// The scope path a symbol binding `b` resolves to through its own handle, as a SQL expression: the
/// raw symbol row first, else the logical group's first member. Either is trusted only if it still
/// answers to the name this store resolved the binding to — the authored name until relocation
/// moves it here (#1297) — because a raw symbol id is a rowid reassigned on reindex and carries no
/// foreign key, so a stale one can name an unrelated live symbol, whose scope would then be
/// published as this target's. NULL, or empty, when the handle is dead or predates the column.
pub(super) const BOUND_SCOPE_PATH_SQL: &str = "COALESCE(
    (SELECT s.scope_path FROM symbols s
      WHERE s.id = b.symbol_id
        AND s.qualified_name_id = (SELECT id FROM name_strings WHERE value = IIF(b.resolved, \
                                               b.resolved_binding_id, b.binding_id))),
    (SELECT s.scope_path FROM logical_symbol_members m
       JOIN symbols s ON s.id = m.symbol_id
       JOIN logical_symbols ls ON ls.id = m.logical_symbol_id
      WHERE m.logical_symbol_id = b.logical_symbol_id
        AND ls.qualified_name_id = (SELECT id FROM name_strings WHERE value = IIF(b.resolved, \
                                               b.resolved_binding_id, b.binding_id))
      ORDER BY m.start_line LIMIT 1))";

/// Whether a projected anchor `a` (a `json_each` row over `anchors_json`) and a binding row `b`
/// name the same row, the same target AND the same publication, as a SQL predicate. The target is
/// compared as the drain's `same_target` compares it — on what identifies it, not where it sits;
/// the location is where the author found it, and says nothing about which target. The publication
/// is `created_at_ms`: each rebind restamps it and `anchors/1` carries it verbatim, so it tells a
/// row a sibling device's newer rebind has yet to reach from the set that device published —
/// which the target columns cannot, when the rebind was between twins agreeing on all of them.
pub(super) const ANCHOR_MATCHES_BINDING_SQL: &str = "json_extract(a.value, '$.binding_kind') = \
                                                     b.binding_kind
    AND json_extract(a.value, '$.binding_id') = b.binding_id
    AND json_extract(a.value, '$.created_at_ms') = b.created_at_ms
    AND json_extract(a.value, '$.symbol_kind') IS b.symbol_kind
    AND json_extract(a.value, '$.signature_hash') IS b.signature_hash
    AND json_extract(a.value, '$.moniker_tool') IS b.moniker_tool
    AND json_extract(a.value, '$.commit_hash') IS b.commit_hash
    AND json_extract(a.value, '$.tracker') IS b.tracker
    AND json_extract(a.value, '$.project') IS b.project
    AND json_extract(a.value, '$.item_key') IS b.item_key";

/// The `NodeAnchorScopes` op for a memory's symbol bindings: each one's live target's scope path,
/// hashed — the identity component the anchor itself leaves out, and the only one that tells two
/// impls of different traits for one type apart when their kind and captured signature agree
/// (#1276). Read through the row's own handle ([`BOUND_SCOPE_PATH_SQL`]), so a binding whose
/// handle is dead, or whose target row predates the scope column, contributes nothing: a scope is
/// published only where it is known, never guessed.
fn anchor_scopes_op(conn: &Connection, memory_id: &str) -> anyhow::Result<MemoryOp> {
    let mut stmt = conn.prepare(&format!(
        "SELECT b.binding_kind, b.binding_id, {BOUND_SCOPE_PATH_SQL}
         FROM repo_memory_bindings b
         WHERE b.memory_id = ?1
           AND b.repo_id = (SELECT repo_id FROM repo_memories WHERE id = ?1)
           AND b.binding_kind IN ('symbol', 'logical_symbol')
         ORDER BY b.binding_kind, b.binding_id"
    ))?;
    let scopes = stmt
        .query_map(params![memory_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .filter_map(|row| {
            row.map(|(binding_kind, binding_id, scope_path)| {
                let scope_path = scope_path.filter(|scope| !scope.is_empty())?;
                Some(rag_rat_oplog::AnchorScope {
                    binding_kind,
                    binding_id,
                    scope_hash: rag_rat_base::hash::hex_sha256(scope_path.as_bytes()),
                })
            })
            .transpose()
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MemoryOp::NodeAnchorScopes { node_id: NodeId::from(memory_id), scopes })
}

/// The `NodeSourceHash` op for a memory's stamped source hash, or `None` when it has none — in
/// which case [`anchor_publication_ops`] publishes an explicit empty hash in its place.
fn source_hash_op(conn: &Connection, memory_id: &str) -> anyhow::Result<Option<MemoryOp>> {
    let mut stmt = conn.prepare("SELECT source_text_hash FROM repo_memories WHERE id = ?1")?;
    let hash: Option<String> = stmt
        .query_map(params![memory_id], |row| row.get::<_, Option<String>>(0))?
        .next()
        .transpose()?
        .flatten();
    Ok(hash.map(|source_text_hash| MemoryOp::NodeSourceHash {
        node_id: NodeId::from(memory_id),
        source_text_hash,
    }))
}

/// The `NodeAnchors` op for a memory's current bindings, or `None` when it has none.
///
/// An unanchored memory authors NOTHING rather than an empty set. The two are different facts to a
/// receiver — nobody published bindings, versus the author saying there are none — but neither
/// seeds anything, so publishing the empty case would cost a signed entry per unanchored memory to
/// tell a peer something it cannot act on. The projection keeps the distinction because a future op
/// that RETRACTS a binding set will need it.
///
/// Deliberately unfiltered: the author publishes every portable fact it holds, including kinds this
/// binary's own drain declines to seed. Which anchors are usable is the receiver's judgment, and
/// filtering here would destroy information a later receiver could use.
fn anchors_op(conn: &Connection, memory_id: &str) -> anyhow::Result<Option<MemoryOp>> {
    let anchors = portable_anchors_of(conn, memory_id)?;
    if anchors.is_empty() {
        return Ok(None);
    }
    Ok(Some(MemoryOp::NodeAnchors { node_id: NodeId::from(memory_id), anchors }))
}

/// Read a memory's bindings as the portable facts the wire carries — every replicated column, and
/// no checkout-local resolution state.
pub(super) fn portable_anchors_of(
    conn: &Connection,
    memory_id: &str,
) -> anyhow::Result<Vec<rag_rat_oplog::PortableAnchor>> {
    let mut stmt = conn.prepare(
        "SELECT binding_kind, binding_id, path, start_line, end_line, commit_hash, tracker,
                project, item_key, created_at_ms, symbol_kind, signature_hash, moniker_tool,
                moniker_tool_version
         FROM repo_memory_bindings
         WHERE memory_id = ?1
           AND repo_id = (SELECT repo_id FROM repo_memories WHERE id = ?1)
         ORDER BY binding_kind, binding_id",
    )?;
    let rows = stmt.query_map(params![memory_id], |row| {
        Ok(rag_rat_oplog::PortableAnchor {
            binding_kind: row.get(0)?,
            binding_id: row.get(1)?,
            path: row.get(2)?,
            start_line: row.get(3)?,
            end_line: row.get(4)?,
            commit_hash: row.get(5)?,
            tracker: row.get(6)?,
            project: row.get(7)?,
            item_key: row.get(8)?,
            created_at_ms: row.get(9)?,
            symbol_kind: row.get(10)?,
            signature_hash: row.get(11)?,
            moniker_tool: row.get(12)?,
            moniker_tool_version: row.get(13)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Author a live memory UPDATE inside the caller's mutation txn: a `NodeUpdate` ONLY when the
/// content actually changed, plus a `NodeStatus` ONLY when the status changed (even to `active`,
/// since the fold needs an explicit op to override a prior non-active status). Content and status
/// are INDEPENDENT LWW registers, so a status-only change must NOT emit a `NodeUpdate` — in a
/// synced multi-writer stream that lifecycle op would re-assert this device's content snapshot at a
/// new Lamport and could revert a concurrent body/title edit from another device. An unknown new
/// status token errors (the write path validates status first, so this is defensive).
pub(crate) fn author_update(
    tx: &Transaction<'_>,
    memory: &RepoMemory,
    content_changed: bool,
    status_changed: bool,
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let node_id = NodeId::from(memory.memory_id.as_str());
    let mut ops = Vec::new();
    if content_changed {
        ops.push(MemoryOp::NodeUpdate {
            node_id: node_id.clone(),
            content: node_content_of_memory(memory),
        });
    }
    if status_changed {
        let status = NodeStatus::from_db_str(&memory.status).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown status token `{}` (a newer binary must author this)",
                memory.status
            )
        })?;
        ops.push(MemoryOp::NodeStatus { node_id, status });
    }
    author_in_owner_stream(tx, &ops, prepared, now_ms)
}

/// Author a live edge ADD (`EdgeAdd`) inside the caller's mutation txn — presence + the durable
/// spec only (no `Rebind`; edge resolution is per-device, recomputed on read).
pub(crate) fn author_edge_add(
    tx: &Transaction<'_>,
    edge: EdgeSpec,
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    author_in_owner_stream(tx, &[MemoryOp::EdgeAdd { edge }], prepared, now_ms)
}

/// Author a live edge REMOVE (`EdgeRemove` tombstone) inside the caller's mutation txn.
pub(crate) fn author_edge_remove(
    tx: &Transaction<'_>,
    edge_key: &str,
    prepared: Option<&PreparedOwnerAuthoring>,
    now_ms: i64,
) -> anyhow::Result<()> {
    author_in_owner_stream(
        tx,
        &[MemoryOp::EdgeRemove { edge_key: EdgeKey::from(edge_key) }],
        prepared,
        now_ms,
    )
}

#[cfg(test)]
#[path = "authoring_tests.rs"]
mod tests;
