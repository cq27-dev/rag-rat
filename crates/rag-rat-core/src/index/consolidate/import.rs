//! Copy every authored + expensive row from `source` into `target` under `repo_id`, in ONE
//! IMMEDIATE transaction (all-or-nothing; the SQLite write lock is taken up front instead of on a
//! mid-transaction upgrade). Every SOURCE table is guarded by presence so a legacy DB predating a
//! feature (e.g. `embedding_cache`, added later than the memory tables) is imported for what it
//! does have rather than erroring.
//!
//! THE MIRROR INVARIANT: until the archive rename
//! lands, the legacy DB is AUTHORITATIVE for this repo's imported slice — keyless resolution
//! keeps serving the legacy file, so any edit in the window between a committed import and a
//! failed rename happens THERE. Every artifact the import copies is therefore REFRESHED to match
//! the source on every run; after the rename, the `.imported` latch makes re-runs unreachable.
//! Per-artifact disposition (every copied artifact MUST appear here and obey the invariant —
//! a new artifact gets classified at birth):
//!  * `repo_memories` (parents)          — content-gated UPSERT ([`copy_memories`]); a no-edit
//!    retry writes nothing, foreign rows are never updated (ownership rides the gate).
//!  * children of ALL mapped ids         — REPLACED unconditionally ([`refresh_children`]): the
//!    (tags/bindings/call-paths/edges)     children of every mapped target id — same-repo AND
//!    remapped — are deleted, then reinserted from the source. Unconditional because a parent-edit
//!    gate needs a "children changed ⇒ parent row changed" signal (updated_at_ms chaining) that is
//!    true today but brittle — the batch-8 gate already missed remapped parents. Replace-in-txn is
//!    convergent and signal-free. Counts stay honest via a before/after slice DIGEST per table:
//!    identical slice ⇒ 0, else the reinserted rows.
//!  * `repo_memory_fts`                  — re-derived for the WHOLE repo at the end
//!    ([`meta_merge::rebuild_memory_fts_for_repo`]); covers refreshed same-repo AND remapped rows
//!    alike (both are stamped `repo_id` = ours by the copy).
//!  * `repo_meta` portable state         — model-state keys use per-key MIRROR
//!    ([`meta_merge::copy_model_state`]); the seal policy uses a monotonic merge so privacy intent
//!    cannot be downgraded by an absent or unsafe source value.
//!  * model-state mirror details: upsert for keys present in the source, DELETE for carried keys
//!    absent there (a model switch in the window may legitimately remove a key, e.g. the remote
//!    config when moving to a local model — keeping it would tear the unit).
//!  * `ai_models` readiness              — restore-style carry
//!    (`meta_merge::carry_active_model_readiness`), re-derived from the SOURCE's active model each
//!    run, so a window model change restores the NEW model's readiness on retry; an explicit
//!    machine-level `disabled` is never overridden.
//!  * `embedding_cache`                  — `INSERT OR IGNORE`, the ONE legitimate IGNORE: rows are
//!    CONTENT-ADDRESSED (`(input_hash, model_id)` determines the vector bytes), so an existing row
//!    is by definition identical and a "stale" extra row is harmless cache that re-embedding never
//!    consults incorrectly.
//!
//! Ends by re-deriving the `repo_memory_fts` mirror for the repo: the copies write the base
//! tables directly, and `memory_search` retrieves EXCLUSIVELY through the FTS mirror — without
//! this, imported memories would be permanently invisible to keyword search (no reconcile/index
//! path repairs the mirror).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use rag_rat_db::storage::IndexConnection;
use rusqlite::{Connection, OptionalExtension, params};

use super::{ImportCounts, MEMORY_STREAM_SEAL_POLICY_META_KEY, copy_children, meta_merge};
use crate::index::{self, schema};

/// Which caller is driving [`import_from_source`], and thus what the source is and what to carry.
/// The two callers make opposite assumptions about the source that every SELECT depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImportMode {
    /// Forward-migrate a legacy per-repo DB into the global store. The source is SINGLE-repo by
    /// construction, so no repo filter; migrate its own rows under the new identity, and carry the
    /// machine-local model state + embedding cache (same-machine migration). "Its own" means
    /// `local` rows plus the `synced` ones the source's own account created on another device:
    /// the reconcile signs every carried row as the target's, so a row ANOTHER account created is
    /// left to come back through sync (#1284).
    ConsolidateLegacy,
    /// Seed a public knowledge-base node from ONE repo's authored memories in a MULTI-repo global
    /// source. Scope the source read to `repo_id` (else other private repos leak onto a public
    /// node), copy only `origin='local'` rows (never re-publish a peer's synced content as our
    /// own), and carry NO machine state (the node runs on a different machine and establishes
    /// its own model + re-embeds under it).
    SeedPublic,
}

/// Import the source slice under the mirror contract documented by this module.
pub(super) fn import_from_source(
    source: &Connection,
    target: &Connection,
    repo_id: &str,
    mode: ImportMode,
) -> anyhow::Result<ImportCounts> {
    let tx =
        rusqlite::Transaction::new_unchecked(target, rusqlite::TransactionBehavior::Immediate)?;
    // CHILD-OWNERSHIP INVARIANT: `copy_memories` returns the source→target MEMORY-ID MAP, and every
    // child copy inserts ONLY under a mapped target id — an id this import verified it owns under
    // `repo_id` this run. A child row must never attach to a parent the import does not own: a
    // legacy memory id colliding with ANOTHER repo's memory in the global store would otherwise
    // have its memory dropped while its tags/bindings/call-paths silently contaminate the other
    // repo's memory.
    let own = match mode {
        ImportMode::ConsolidateLegacy => source_created_content(source)?,
        ImportMode::SeedPublic => rag_rat_oplog::CreatedContent::default(),
    };
    // A sealed entry this store cannot open hides which synced rows its account created, and a
    // successful run archives the legacy file: refuse rather than leave that account's memories
    // behind.
    anyhow::ensure!(
        own.unreadable == 0,
        "the legacy index holds {} memory entries its own account created that this device cannot \
         read (sealed without a key here, or corrupt), so consolidation cannot tell which synced \
         memories are its own; refusing rather than leave them behind (sync with a device that \
         holds the stream key, then retry)",
        own.unreadable
    );
    // The import drops a synced memory another account created, and everything attached to it.
    // Work the source's own account did on such a memory — an edit, a rebind, an edge added from
    // it (signed, or local and not signed yet) — cannot be carried without republishing that
    // memory as the target's own, and a successful run archives the legacy file: refuse instead.
    // Seed carries no synced row and archives nothing, and its source is a shared multi-repo store:
    // the guard is legacy consolidation's alone.
    let stranded = match mode {
        ImportMode::ConsolidateLegacy => stranded_own_work(source, &own)?,
        ImportMode::SeedPublic => 0,
    };
    anyhow::ensure!(
        stranded == 0,
        "the legacy index holds {stranded} changes its own account made to memories another \
         account created (edits, rebinds or edges from them); consolidation cannot carry them \
         without republishing those memories as this store's own, so it refuses rather than drop \
         them"
    );
    let CopiedMemories { rows_written: memories, id_map } =
        copy_memories(source, &tx, repo_id, mode, &own)?;
    // Mirror invariant: children of EVERY mapped id are replaced, not unioned. Digest the target's
    // child slices before and after — identical slice ⇒ that table reports 0 (an honest no-op).
    let pre = child_slice_digests(&tx, &id_map)?;
    refresh_children(&tx, repo_id, &id_map)?;
    let callee_remap = consolidation_callee_remap(source, repo_id)?;
    let raw = ImportCounts {
        memories,
        bindings: copy_children::copy_bindings(source, &tx, repo_id, &id_map)?,
        tags: copy_children::copy_tags(source, &tx, &id_map)?,
        call_paths: copy_children::copy_call_paths(source, &tx, &id_map)?,
        call_path_edges: copy_children::copy_call_path_edges(source, &tx, &id_map)?,
        edges: copy_children::copy_node_edges(source, &tx, repo_id, &id_map, mode, &own)?,
        // Seed carries NO machine state: the embedding cache is content-derived from other private
        // repos on this box, and the model-state meta names this machine's embedder — the public
        // node runs elsewhere and establishes its own (see `ImportMode::SeedPublic`).
        embedding_cache_rows: match mode {
            ImportMode::ConsolidateLegacy => meta_merge::copy_embedding_cache(source, &tx)?,
            ImportMode::SeedPublic => 0,
        },
        meta_keys: match mode {
            ImportMode::ConsolidateLegacy => meta_merge::copy_model_state(source, &tx, repo_id)?,
            ImportMode::SeedPublic => 0,
        },
    };
    rag_rat_query::memory::remap_call_path_callee_logical_symbol_ids(&tx, source, &callee_remap)?;
    let post = child_slice_digests(&tx, &id_map)?;
    let counts = ChildSliceDigests::zero_unchanged(&pre, &post, raw);
    meta_merge::rebuild_memory_fts_for_repo(&tx, repo_id)?;
    tx.commit()?;
    Ok(counts)
}

/// Refuse to seed a public node from a source repo that authors SEALED memories. A sealed source is
/// a strong "keep encrypted" signal and publishing is a one-way ratchet, so fail BEFORE publish
/// commits rather than silently republishing sealed-at-rest content as world-readable plaintext
/// (the bodies live plaintext in `repo_memories` regardless). Absent seal meta ⇒ proceed. Any
/// present value refuses (`sealed`, or an unknown future token — a public node accepts only a
/// plainly unsealed source).
pub(crate) fn ensure_source_unsealed(source_path: &Path, repo_id: &str) -> anyhow::Result<()> {
    let source = IndexConnection::open_read_only_blocking(source_path)
        .with_context(|| format!("opening seed index {} read-only", source_path.display()))?;
    if !schema::table_exists(source.connection(), "repo_meta")? {
        return Ok(());
    }
    let policy: Option<String> = source
        .connection()
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id = ?1 AND key = ?2",
            params![repo_id, MEMORY_STREAM_SEAL_POLICY_META_KEY],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(policy) = policy {
        anyhow::bail!(
            "seed source repo `{repo_id}` authors sealed memories (policy `{policy}`); a public \
             knowledge base can only be seeded from a plaintext source — publishing sealed \
             content to anonymous readers is refused"
        );
    }
    Ok(())
}

/// Seed a freshly-published public node from ONE repo's locally-authored memories in `source_path`
/// (a multi-repo global store), then author them onto the node's PublicRead owner stream. Reuses
/// the consolidation import under [`ImportMode::SeedPublic`] — scoped to `repo_id`,
/// `origin='local'` only, no machine-state carry. The caller must have already refused a sealed
/// source ([`ensure_source_unsealed`]) and published the node
/// ([`crate::memory_write::enable_public_authoring`]) — publish sets the access-mode intent that
/// makes the reconcile below resolve the PublicRead `/2` stream.
pub(crate) fn seed_from_index(
    target: &Connection,
    source_path: &Path,
    repo_id: &str,
    now_ms: i64,
) -> anyhow::Result<u64> {
    let source = IndexConnection::open_read_only_blocking(source_path)
        .with_context(|| format!("opening seed index {} read-only", source_path.display()))?;
    // One deferred read snapshot over the whole import: the source is the operator's LIVE index
    // (watcher/MCP may be writing), so a concurrent write must not tear the parent/child copies.
    let snapshot = source.connection().unchecked_transaction()?;
    let counts = import_from_source(&snapshot, target, repo_id, ImportMode::SeedPublic)?;
    drop(snapshot);
    drop(source);
    // Idempotent guard: ensure the store-global `/3` projection is current before the reconcile
    // anti-join trusts it (no-op when already fresh).
    rag_rat_oplog::rebuild_all_content_projections_if_stale(target)?;
    // Author the seeded `origin='local'` rows onto the owner stream — PublicRead, because publish
    // set the access-mode intent the stream resolver reads.
    crate::memory_write::reconcile_owner_stream_for_repo(target, repo_id, now_ms)?;
    Ok(counts.memories)
}

/// Re-derive every persisted call-path callee id under the destination repo identity. The source
/// graph remains the fingerprint evidence during import because derived graph rows are deliberately
/// not copied into the consolidated store.
struct ConsolidationLogicalSymbolFields {
    language: String,
    path: String,
    name: String,
    qualified_name: Option<String>,
    kind: String,
}

fn consolidation_callee_remap(
    source: &Connection,
    repo_id: &str,
) -> anyhow::Result<Vec<(i64, Option<i64>)>> {
    if !schema::column_exists(source, "repo_memory_call_path_edges", "callee_logical_symbol_id")? {
        return Ok(Vec::new());
    }
    let mut stmt = source.prepare(
        "SELECT DISTINCT callee_logical_symbol_id
           FROM repo_memory_call_path_edges
          WHERE callee_logical_symbol_id IS NOT NULL",
    )?;
    let ids =
        stmt.query_map([], |row| row.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut remap = Vec::with_capacity(ids.len());
    for old_id in ids {
        let fields = source
            .query_row(
                "SELECT ls.language, ls.path, ls.logical_name, qn.value, ls.kind
                   FROM logical_symbols ls
                   LEFT JOIN name_strings qn ON qn.id = ls.qualified_name_id
                  WHERE ls.id = ?1",
                [old_id],
                |row| {
                    Ok(ConsolidationLogicalSymbolFields {
                        language: row.get(0)?,
                        path: row.get(1)?,
                        name: row.get(2)?,
                        qualified_name: row.get(3)?,
                        kind: row.get(4)?,
                    })
                },
            )
            .optional()?;
        let key = match fields {
            Some(fields) => consolidation_logical_symbol_key(source, old_id, fields)?,
            None => None,
        };
        remap.push((old_id, key.map(|key| key.stable_id(repo_id))));
    }
    Ok(remap)
}

/// Recover a portable key only when every member agrees on the member-resident key fields. A
/// legacy source can hold a pre-scope-aware merged group; choosing one member would silently assign
/// every persisted callee reference to that arbitrary owner after consolidation.
fn consolidation_logical_symbol_key(
    source: &Connection,
    old_id: i64,
    fields: ConsolidationLogicalSymbolFields,
) -> anyhow::Result<Option<index::graph_index::LogicalSymbolKey>> {
    let Some(qualified_name) = fields.qualified_name else { return Ok(None) };
    let mut stmt = source.prepare(
        "SELECT COALESCE(s.scope_path, ''), s.signature
           FROM logical_symbol_members m
           JOIN symbols s ON s.id = m.symbol_id
          WHERE m.logical_symbol_id = ?1
          ORDER BY s.id",
    )?;
    let members = stmt
        .query_map([old_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some((scope_path, signature)) = members.first().cloned() else { return Ok(None) };
    if members.iter().any(|(member_scope, member_signature)| {
        member_scope != &scope_path || member_signature != &signature
    }) {
        return Ok(None);
    }
    Ok(Some(index::graph_index::LogicalSymbolKey {
        language: fields.language,
        path: fields.path,
        name: fields.name,
        qualified_name,
        scope_path,
        kind: fields.kind,
        signature,
    }))
}

/// One SHA-256 digest per child table over the TARGET rows of the mapped ids (order-insensitive:
/// rows are serialized with type tags and sorted before hashing). Drives the honest-count gate in
/// [`import_from_source`]: replace-then-reinsert genuinely rewrites rows on every run, but a run
/// that leaves a slice byte-identical did no work worth reporting.
struct ChildSliceDigests(BTreeMap<&'static str, [u8; 32]>);

struct ChildSlice {
    table: &'static str,
    id_column: &'static str,
    repo_scoped: bool,
    count: fn(&mut ImportCounts) -> &mut u64,
}

// Preserve child-deletion order. Bindings alone additionally scope by repo; node edges are
// owned by source_node_id, so neither exception can disappear when a child slice is added.
const CHILD_SLICES: &[ChildSlice] = &[
    ChildSlice {
        table: "repo_memory_tags",
        id_column: "memory_id",
        repo_scoped: false,
        count: |counts| &mut counts.tags,
    },
    ChildSlice {
        table: "repo_memory_call_paths",
        id_column: "memory_id",
        repo_scoped: false,
        count: |counts| &mut counts.call_paths,
    },
    ChildSlice {
        table: "repo_memory_call_path_edges",
        id_column: "memory_id",
        repo_scoped: false,
        count: |counts| &mut counts.call_path_edges,
    },
    ChildSlice {
        table: "repo_memory_bindings",
        id_column: "memory_id",
        repo_scoped: true,
        count: |counts| &mut counts.bindings,
    },
    ChildSlice {
        table: "repo_node_edges",
        id_column: "source_node_id",
        repo_scoped: false,
        count: |counts| &mut counts.edges,
    },
];

impl ChildSliceDigests {
    fn zero_unchanged(pre: &Self, post: &Self, mut raw: ImportCounts) -> ImportCounts {
        for slice in CHILD_SLICES {
            if pre.0[slice.table] == post.0[slice.table] {
                *(slice.count)(&mut raw) = 0;
            }
        }
        raw
    }
}

fn child_slice_digests(
    tx: &Connection,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<ChildSliceDigests> {
    CHILD_SLICES
        .iter()
        .map(|slice| {
            Ok((slice.table, child_slice_digest(tx, slice.table, slice.id_column, id_map)?))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()
        .map(ChildSliceDigests)
}

fn child_slice_digest(
    tx: &Connection,
    table: &str,
    id_column: &str,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    // `table` / `id_column` are compile-time constants at every call site, never user input.
    let mut stmt = tx.prepare(&format!("SELECT * FROM {table} WHERE {id_column} = ?1"))?;
    let mut lines: Vec<String> = Vec::new();
    for target_id in id_map.values() {
        let mut rows = stmt.query([target_id])?;
        while let Some(row) = rows.next()? {
            let mut line = String::new();
            for i in 0..row.as_ref().column_count() {
                match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => line.push_str("|n"),
                    rusqlite::types::ValueRef::Integer(v) => {
                        line.push_str(&format!("|i{v}"));
                    },
                    rusqlite::types::ValueRef::Real(v) => line.push_str(&format!("|r{v}")),
                    rusqlite::types::ValueRef::Text(v) => {
                        line.push_str(&format!("|t{}", String::from_utf8_lossy(v)));
                    },
                    rusqlite::types::ValueRef::Blob(v) => {
                        line.push_str(&format!("|b{}", v.len()));
                        line.push_str(&rag_rat_base::hash::hex_lower(v));
                    },
                }
            }
            lines.push(line);
        }
    }
    lines.sort_unstable();
    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line.as_bytes());
        hasher.update([0u8]);
    }
    Ok(hasher.finalize().into())
}

/// Copy `repo_memories`, stamping the target `repo_id`, and return `(rows written, source→target
/// id map)`. Selects only the STABLE portable columns (never the source `repo_id`), so it reads
/// faithfully even from a legacy DB predating `repo_memories.repo_id` (V042). COLUMN
/// CLASSIFICATION (the audit rule for every copy below): every column is either PORTABLE (copied
/// verbatim) or a LOCAL ROWID (NULLed for re-resolution) — a new column added to one of these
/// tables must be consciously classified into one of the two, never silently dropped by a partial
/// SELECT.
///
/// ID COLLISIONS: memory ids are TEXT and only unique per DB, so a second legacy import can carry
/// an id the global store already holds. Three cases:
///  * unclaimed → insert under the ORIGINAL id (ids are referenced in prose; keep them when free);
///  * owned by THIS repo → the CRASH-RETRY case (Codex batch 8): the import txn committed but the
///    rename failed, the legacy file stayed the LIVE store (keyless resolution keeps serving it
///    until the rename lands), and the user may have edited the memory there. The write is an
///    honest UPSERT — the legacy content REPLACES the stale global copy, gated on an actual content
///    difference (row-value `IS NOT`) so a no-edit retry writes nothing and the counts stay honest.
///    The gate also requires `repo_id` ownership, so a foreign row is never updated.
///  * owned by a DIFFERENT repo → REMAP to [`remapped_memory_id`] (deterministic, so a retry
///    converges on the same id) and import under the new id — never drop the memory, never let its
///    children attach to the other repo's row.
///
/// Children are NOT gated here: [`import_from_source`] replaces the children of EVERY mapped id
/// unconditionally (see the mirror invariant there) — a parent-edit gate would need the
/// "children changed ⇒ parent bumped" signal, which the remap path already broke once.
///
/// After every insert the target row's ownership is VERIFIED (`repo_id` must be ours) — the
/// structural backstop for the child-ownership invariant in [`import_from_source`].
/// How many pieces of the source account's own work sit on a synced memory the import leaves out
/// (#1284): memories that account's ops touch without having created them, plus local edges from
/// such a memory (added here, possibly not signed yet).
fn stranded_own_work(
    source: &Connection,
    own: &rag_rat_oplog::CreatedContent,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_memories")? {
        return Ok(0);
    }
    let own_nodes = serde_json::to_string(&own.nodes)?;
    let touched_foreign: i64 = source.query_row(
        "SELECT COUNT(*) FROM repo_memories
         WHERE origin = 'synced' AND id IN (SELECT value FROM json_each(?1))
           AND id NOT IN (SELECT value FROM json_each(?2))",
        params![serde_json::to_string(&own.touched)?, own_nodes],
        |row| row.get(0),
    )?;
    let local_edges_from_foreign: i64 = if schema::table_exists(source, "repo_node_edges")? {
        source.query_row(
            "SELECT COUNT(*) FROM repo_node_edges e JOIN repo_memories m ON m.id = \
             e.source_node_id
             WHERE e.origin = 'local' AND m.origin = 'synced'
               AND m.id NOT IN (SELECT value FROM json_each(?1))",
            params![own_nodes],
            |row| row.get(0),
        )?
    } else {
        0
    };
    Ok(u64::try_from(touched_foreign + local_edges_from_foreign)?)
}

/// What the legacy source's own account created, read from its accepted `/3` content — the
/// `synced` rows the import may carry as the target's own (#1284). Empty when the source never
/// minted an account.
fn source_created_content(source: &Connection) -> anyhow::Result<rag_rat_oplog::CreatedContent> {
    if !schema::table_exists(source, "content_entries")?
        || !schema::table_exists(source, "oplog_local_account")?
    {
        return Ok(rag_rat_oplog::CreatedContent::default());
    }
    match rag_rat_oplog::read_local_account(source)? {
        Some(account) => rag_rat_oplog::content_created_by(source, account),
        None => Ok(rag_rat_oplog::CreatedContent::default()),
    }
}

fn copy_memories(
    source: &Connection,
    tx: &Connection,
    repo_id: &str,
    mode: ImportMode,
    own: &rag_rat_oplog::CreatedContent,
) -> anyhow::Result<CopiedMemories> {
    if !schema::table_exists(source, "repo_memories")? {
        return Ok(CopiedMemories::default());
    }
    const BASE: &str = "SELECT id, kind, title, body, confidence, status, created_by, \
                        created_at_ms, updated_at_ms, source, source_text_hash, input_hash, \
                        memory_version, payload_json FROM repo_memories";
    // The reconcile signs every imported row onto the target's own stream, so a `synced` row is
    // carried only when the source's own account created it (`own`, empty for seed): a row another
    // account created would be republished as the target's authorship (#1284). Seed also scopes
    // the read to the ONE published repo (a multi-repo source would otherwise leak other private
    // repos onto a public node); legacy consolidation's source is single-repo.
    let sql = match mode {
        ImportMode::ConsolidateLegacy =>
            format!("{BASE} WHERE origin = 'local' OR id IN (SELECT value FROM json_each(?1))"),
        ImportMode::SeedPublic => format!("{BASE} WHERE repo_id = ?1 AND origin = 'local'"),
    };
    let own_nodes = serde_json::to_string(&own.nodes)?;
    let mut stmt = source.prepare(&sql)?;
    let mut rows = match mode {
        ImportMode::ConsolidateLegacy => stmt.query(params![own_nodes])?,
        ImportMode::SeedPublic => stmt.query(params![repo_id])?,
    };
    let mut count = 0u64;
    let mut id_map: BTreeMap<String, String> = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let source_id = row.get::<_, String>(0)?;
        let target_id = match memory_owner(tx, &source_id)? {
            Some(owner) if owner != repo_id => remapped_memory_id(repo_id, &source_id),
            _ => source_id.clone(),
        };
        let changed = tx.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, source_text_hash, input_hash, memory_version, \
             payload_json, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(id) DO UPDATE SET
               kind = excluded.kind, title = excluded.title, body = excluded.body,
               confidence = excluded.confidence, status = excluded.status,
               created_by = excluded.created_by, created_at_ms = excluded.created_at_ms,
               updated_at_ms = excluded.updated_at_ms, source = excluded.source,
               source_text_hash = excluded.source_text_hash, input_hash = excluded.input_hash,
               memory_version = excluded.memory_version, payload_json = excluded.payload_json
             WHERE repo_memories.repo_id = excluded.repo_id
               AND (repo_memories.kind, repo_memories.title, repo_memories.body, \
             repo_memories.confidence, repo_memories.status, repo_memories.created_by, \
             repo_memories.created_at_ms, repo_memories.updated_at_ms, repo_memories.source, \
             repo_memories.source_text_hash, repo_memories.input_hash, \
             repo_memories.memory_version, repo_memories.payload_json)
               IS NOT (excluded.kind, excluded.title, excluded.body, excluded.confidence, \
             excluded.status, excluded.created_by, excluded.created_at_ms, \
             excluded.updated_at_ms, excluded.source, excluded.source_text_hash, \
             excluded.input_hash, excluded.memory_version, excluded.payload_json)",
            params![
                target_id,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, Option<String>>(13)?,
                repo_id,
            ],
        )?;
        // The structural invariant: whatever happened above, the mapped target id must now be OURS
        // — else children keyed to it would attach to another repo's memory. Unreachable in
        // practice (the remap made the id collision-free), but a violated invariant here must be a
        // hard error, never silent contamination.
        if memory_owner(tx, &target_id)?.as_deref() != Some(repo_id) {
            anyhow::bail!(
                "memory id {target_id} is not owned by {repo_id} after import — refusing to \
                 attach its children across repos"
            );
        }
        count += changed as u64;
        id_map.insert(source_id, target_id);
    }
    Ok(CopiedMemories { rows_written: count, id_map })
}

/// [`copy_memories`]' result: rows actually written, and the source→target id map every child
/// copy keys off.
#[derive(Default)]
struct CopiedMemories {
    rows_written: u64,
    id_map: BTreeMap<String, String>,
}

/// Delete the CHILD rows (tags, bindings, call-paths, call-path edges) of EVERY mapped target id
/// — same-repo and remapped alike — inside the import transaction, so the subsequent child copies
/// reinsert the legacy state wholesale (the mirror invariant in [`import_from_source`]): the
/// legacy is the live store until the rename lands, so its child sets REPLACE the stale global
/// ones (a tag removed legacy-side must not survive by union). Table presence is not probed:
/// these are TARGET tables, always at current schema.
fn refresh_children(
    tx: &Connection,
    repo_id: &str,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    for id in id_map.values() {
        for slice in CHILD_SLICES {
            let sql = format!("DELETE FROM {} WHERE {} = ?1", slice.table, slice.id_column);
            if slice.repo_scoped {
                tx.execute(&format!("{sql} AND repo_id = ?2"), params![id, repo_id])?;
            } else {
                tx.execute(&sql, [id])?;
            }
        }
    }
    Ok(())
}

/// The `repo_id` owning memory `id` in the target DB, or `None` when the id is unclaimed.
fn memory_owner(conn: &Connection, id: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT repo_id FROM repo_memories WHERE id = ?1", [id], |row| {
            row.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

/// The DETERMINISTIC replacement id for a legacy memory whose original id is owned by a DIFFERENT
/// repo in the global store: `sha256(repo_id ‖ 0x00 ‖ original_id)`, rendered in the native
/// `mem_<hex>_<hex>` shape. Deterministic so an import retry (a rename that failed mid-run)
/// converges on the SAME remapped id instead of minting duplicates.
fn remapped_memory_id(repo_id: &str, original_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(repo_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(original_id.as_bytes());
    let hex: String = rag_rat_base::hash::hex_lower(&hasher.finalize());
    format!("mem_{}_{}", &hex[..13], &hex[13..25])
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
