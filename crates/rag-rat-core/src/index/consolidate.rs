//! `rag-rat consolidate` (memory-sync phase A7): import a repo's legacy per-repo index into the
//! consolidated GLOBAL database, then rename the legacy file out of the way so the config default
//! resolves to the global store from then on.
//!
//! What is imported is exactly the AUTHORED and EXPENSIVE data — `repo_memories` (with their
//! bindings / tags / call-paths / call-path edges), the content-addressed `embedding_cache`, and
//! the durable model-identity `repo_meta` keys. Everything else is DERIVED (chunks, symbols, edges,
//! FTS, …) and is cheaper and safer to rebuild than to translate across rowid spaces — so a fresh
//! index of the consolidated repo regenerates it, and the carried `embedding_cache` makes the
//! re-embedding a no-op (its `input_hash` folds model + version + input text).
//!
//! POSTURE (spec §3.4):
//! - Portable bindings only: the memory bindings' LOCAL rowid columns (`logical_symbol_id` /
//!   `symbol_id` / `chunk_id` / `edge_id`) are NULLed on import — those rowids mean nothing in a
//!   fresh index — and the normal validate loop re-resolves them from the portable anchor (path /
//!   commit / tracker / moniker fields, which ARE copied verbatim) after the next index pass.
//! - `live_files_generation` is NOT carried (absent ⇒ 0 is load-bearing: a fresh index of the
//!   consolidated repo stages above 0 and flips normally — A6 handoff rule #1).
//! - Idempotent AND crash-honest: a no-edit retry writes nothing (content-gated upserts / `INSERT
//!   OR IGNORE`), and a retry after a crashed RENAME carries legacy-side edits made in the window
//!   forward — the legacy file is the live store until the rename lands, so its content REPLACES
//!   stale same-repo global copies (children included). Once the legacy file is renamed to
//!   `index.sqlite.imported` a re-run is a no-op with a notice.
//! - LOCKED: the whole registration → import → rename sequence runs under the repo's per-repo write
//!   locks — on the TARGET side (a writer's held lock must match the repo id it writes under, the
//!   A6 structural rule) AND on the SOURCE side (a watcher / MCP server still pointed at the legacy
//!   file keys its lock beside THAT path; holding it means no lock-disciplined writer can append to
//!   the legacy DB between the snapshot read and the rename — writes there would otherwise silently
//!   vanish into the renamed artifact).
//! - An explicit `[index] database` key is REFUSED outright (no import, no side effects): renaming
//!   the file out from under the still-pinned config would strand it on a fresh empty DB, and an
//!   early import WITHOUT the rename would open a DELIBERATE divergence window with the global
//!   store reachable, which the refusal exists to prevent (the crash-retry upsert covers the
//!   accidental window a failed rename creates). A pinned config never reads the global store, so
//!   an early import has no value; the refusal names the remedy (remove the key, re-run) and the
//!   single completing run imports and renames atomically with no window.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

use crate::config::{self, Config};
use crate::index::{IndexDatabase, schema};
use crate::repo_identity::{self, RepoIdentity};
use crate::storage::IndexConnection;
use crate::{data_dir, locks};

/// The `repo_meta` keys consolidate carries from the legacy DB into the global DB. Carried as a
/// per-key MIRROR of the source (the import's mirror invariant): a key present in the legacy DB
/// upserts (value-gated, so an unchanged retry writes nothing), a carried key ABSENT there
/// deletes any stale target row — the legacy DB is the live store until the rename lands, so a
/// model switch made in the crash-retry window (including one that removes a key, e.g. dropping
/// the remote config when moving to a local model) must win over the previous run's copy.
/// Nothing else writes these keys for a mid-consolidate repo (the import holds the repo's write
/// locks and the target migrate is schema-only), so there is no competing "config-seeded" value
/// to preserve.
///
/// CLASSIFICATION RULE (every `repo_meta` key must land in exactly one class — classify a NEW key
/// here at birth):
///
///  (a) REPO-PORTABLE CONFIGURATION — durable "how this repo embeds" identity/state that must
///      survive the move or the carried `embedding_cache` / remote transport is stranded → COPIED
///      (as ONE model-state unit — see [`copy_model_state`], which also carries the active
///      model's `ai_models` readiness row):
///      * `active_embedding_model` — which embedder the cache rows belong to;
///      * `embedding_active_model_version` — the active model's freshness key;
///      * `active_embedding_remote_config` — the persisted remote-endpoint config
///        `active_embedder()` reconstructs its query/connect-mode transport from; dropping it would
///        silently reroute post-consolidation searches to the local backend (or lexical) until the
///        model is reinstalled;
///      * `active_embedding_model_provisional` — SEMANTIC state, not transient: absent reads as
///        NON-provisional (an explicit, config-immune choice), so dropping a set `"1"` would
///        CONVERT an auto-selected model into a confirmed one and `seed_active_embedding_model`
///        could no longer override/clear it from config. ABSENCE-HAS-MEANING keys like this need
///        the absent state classified too, not just the value.
///
///  (b) DB-LOCAL STATE — never copied; each entry states why:
///      * freshness/progress cursors that would make a fresh 0-row index falsely report itself
///        current: `content_revision`, `git_commit`, `git_dirty`, `git_history_indexed_head` /
///        `_root` / `_shallow` / `_complete`, `papertrail_last_sync_ms`, `graph_index_version`,
///        `indexed_at_ms`, `vector_int8_reencode_done` / `_cursor`,
///        `last_embedding_reconcile_started_at_ms` / `_finished_at_ms`;
///      * pointers/state owned by THIS database file's lifecycle: `live_files_generation` (absent ⇒
///        0 is load-bearing — A6), `clone_graph_live_generation`, `shallow_boundary` (adoption
///        proof for the legacy file's own registry), `source_root` (re-recorded by registration);
///      * derived/re-derivable caches: `local_crate_roots` (re-read from manifests),
///        `embedding_throughput_tune_v1` (a tuning cache, re-derived).
const CARRIED_META_KEYS: &[&str] = &[
    "active_embedding_model",
    "embedding_active_model_version",
    "active_embedding_remote_config",
    "active_embedding_model_provisional",
];

/// How long consolidate waits for the repo's per-repo write locks (global-side and legacy-side)
/// before refusing — an in-flight index/maintenance pass finishes well within it, and an explicit
/// retryable refusal beats importing under a live writer.
const CONSOLIDATE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The result of a `rag-rat consolidate` run — the CLI renders it; the core owns the logic.
#[derive(Debug)]
pub enum ConsolidateOutcome {
    /// The repo already resolves to the global database (no `database` key, no legacy file, or an
    /// explicit `database` already pointing at the global store) — nothing to do.
    AlreadyGlobal { database: PathBuf },
    /// A previous run already imported this repo (the `.imported` marker is present) — a no-op.
    AlreadyImported { imported: PathBuf },
    /// No legacy per-repo index exists to import (a fresh repo already on the global default).
    NoLegacyIndex { source: PathBuf },
    /// The legacy index was imported (and, unless the config pins an explicit `database` key,
    /// renamed away).
    Imported(ImportSummary),
}

/// Per-run import counts + the paths involved, for the CLI summary. Counts reflect rows actually
/// WRITTEN — an idempotent re-run over already-imported rows reports zeros, not phantom copies.
#[derive(Debug)]
pub struct ImportSummary {
    pub repo_id: String,
    pub source: PathBuf,
    /// The `index.sqlite.imported` marker the legacy file was renamed to (always — a pinned config
    /// is refused before any import, so a summary only exists for a completed import + rename).
    pub renamed_to: PathBuf,
    pub target: PathBuf,
    pub memories: u64,
    pub bindings: u64,
    pub tags: u64,
    pub call_paths: u64,
    pub call_path_edges: u64,
    pub edges: u64,
    pub embedding_cache_rows: u64,
    pub meta_keys: u64,
}

/// Consolidate the repo of `config` into the global database. The pinned-key refusal keys off
/// `config.database_key_pinned` — the GOVERNING (main-worktree-anchored) key decision
/// `Config::load` already made — so the refusal and the `database` resolution can never disagree (a
/// linked worktree's branch-local toml is not authoritative for either).
pub fn run(config: &Config) -> anyhow::Result<ConsolidateOutcome> {
    let target = data_dir::global_database_path().context(
        "cannot resolve the global database path: no data directory is available (set \
         RAG_RAT_DATA_DIR, XDG_DATA_HOME, or HOME)",
    )?;
    let mut source = config.database.clone();

    // Already on the global store (an explicit `database = <global>` or a keyless config whose
    // legacy file was already imported) — usually nothing to import, with ONE refinement: a
    // config explicitly PINNED AT the global path can coexist with a lingering, never-imported
    // legacy file (the pin was added by hand, so no consolidate run ever renamed the old
    // per-repo DB) — reporting `already_global` there strands the authored memories in the old
    // file while claiming success. Probe the default legacy path: present without its
    // `.imported` marker ⇒ import FROM it. This is the one pinned shape where proceeding is
    // strictly correct — the pin already names the target, so the rename cannot strand the
    // config (post-import it still resolves global), which is why the pinned refusal below is
    // skipped for it.
    let mut pinned_at_target = false;
    if source == target {
        let legacy = config::default_legacy_database_path(&config.root);
        if legacy != target && legacy.exists() && !imported_marker(&legacy).exists() {
            source = legacy;
            pinned_at_target = true;
        } else {
            return Ok(ConsolidateOutcome::AlreadyGlobal { database: target });
        }
    }

    // A pinned `[index] database` key is REFUSED before ANY side effect — and BEFORE the
    // missing-source exits below: a pin at a missing/renamed path would otherwise report a happy
    // `no_legacy_index` / `already_consolidated` while the repo stays stranded on the pin (the
    // next `rag-rat index` recreates an empty per-repo DB there). Only a pin at the global target
    // itself is genuinely fine — that returned `AlreadyGlobal` above. Rationale for refusing at
    // all: renaming the legacy file would strand the still-pinned config on a fresh empty DB, and
    // importing WITHOUT renaming would open a divergence window (memories edited in the
    // still-live legacy DB before a later finishing run are silently dropped by the idempotent
    // `INSERT OR IGNORE`s); a pinned config never reads the global store, so an early import buys
    // nothing.
    if config.database_key_pinned && !pinned_at_target {
        let default_legacy = config::default_legacy_database_path(&config.root);
        anyhow::bail!("{}", pinned_refusal_message(&source, &default_legacy));
    }

    let imported = imported_marker(&source);
    if !source.exists() {
        return Ok(if imported.exists() {
            ConsolidateOutcome::AlreadyImported { imported }
        } else {
            ConsolidateOutcome::NoLegacyIndex { source }
        });
    }

    // Resolve the repo identity FIRST: the per-repo write locks are keyed by the id this run
    // registers and writes under (the A6 lock-matches-written-id rule).
    let identity = resolve_identity_for_config(config)?;

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating the global data directory {}", parent.display()))?;
    }

    // Hold the repo's per-repo write locks for the WHOLE registration → import → rename sequence:
    // the GLOBAL-side lock covers every row written under `identity.repo_id` in the global DB, and
    // the LEGACY-side locks exclude a watcher / MCP writer still keyed beside the legacy file, so
    // nothing can append to it between the snapshot read and the rename. Both bounded; the global
    // locks taken inside (schema in the migration, registry in `register_repo`) follow the
    // per-repo → global ordering rule (see `locks::registry_lock_path`).
    //
    // The LEGACY side drains EVERY id the source DB itself records, not just the CURRENT identity:
    // the legacy file predates the identity transition, so a writer started PRE-deepen still keys
    // its flock by the OLD `local:` id — a current-identity lock alone would not conflict with it,
    // and the snapshot + rename could race its writes into the renamed artifact (the same loss the
    // same-id lock exists to prevent; the outgoing-drain rule the upgrade path follows). The ids
    // come from a read-only PRE-lock peek at the source's own `repos` registry; canonical order,
    // bounded. A source so old it predates the registry tables peeks empty — only pre-A6 binaries
    // (whose lock files predate per-repo keying entirely) ever wrote such a file, so no
    // current-scheme lock could coordinate with them regardless.
    let _target_lock = acquire_consolidate_lock(&target, &identity.repo_id, "global")?;
    let mut source_side_ids = source_registered_repo_ids(&source);
    if !source_side_ids.iter().any(|id| id == &identity.repo_id) {
        source_side_ids.push(identity.repo_id.clone());
    }
    source_side_ids.sort_by(|a, b| {
        if crate::locks::canonical_lock_order(a, b).0 == a.as_str() {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    });
    let mut _source_locks = Vec::with_capacity(source_side_ids.len());
    for id in &source_side_ids {
        _source_locks.push(acquire_consolidate_lock(&source, id, "legacy")?);
    }

    // Bring the global DB to current schema (creating it under the shared schema lock), then open
    // a fresh connection to register this repo and copy rows into it. SCHEMA-ONLY, never the
    // healing `migrate`: this connection is config-less and the SUBJECT repo is not registered
    // yet, so the open-time healers' scoped-repo witness would resolve the sole REGISTERED repo —
    // a SIBLING when consolidating a second repo into a one-repo global DB — and heal ITS
    // `repo_meta` under only the incoming repo's locks (see the witness-limit note on
    // `scoped_repo_witness`). Any owed sibling heal belongs to that sibling's next scoped open.
    IndexDatabase::migrate_schema_only(&target)
        .with_context(|| format!("migrating the global database {}", target.display()))?;
    let target_storage = IndexConnection::open(&target)?;
    let target_conn = target_storage.connection();

    // Register (or adopt/extend) this repo in the global DB. The returned id is what every imported
    // row is stamped with — the legacy DB's own `repo_id` (placeholder or otherwise) is discarded.
    // Consolidation IMPORTS an indexed repo, so `register_repo` records the working-tree root
    // (#427).
    let repo_id = schema::register_repo(target_conn, &identity, &config.root, schema::now_ms())?;

    // Bring the SOURCE to current schema too (read-write, under the source-side locks held above,
    // and about to be renamed `.imported` anyway): an old-vintage legacy DB this binary never
    // opened keeps its model meta in the pre-V039 `index_meta`/`reconcile_meta` tables, and the
    // import below reads `repo_meta` ONLY — without this the embedding cache would carry while
    // the model identity/remote config/provisional flag silently dropped (the model-state unit
    // broken on the vintage axis). Running the ladder lets V039/V040's own migrations do the meta
    // relocation instead of the importer re-implementing dual-path key reads. Schema-only (no
    // heals — single-repo or not, there is nothing an open-time heal should touch on a file being
    // retired). An unmigratable source propagates: it cannot be trusted for import.
    IndexDatabase::migrate_schema_only(&source).with_context(|| {
        format!("migrating the legacy index {} before import", source.display())
    })?;

    // Fold any WAL content into the legacy main file before the snapshot read, so the file the
    // rename moves is self-contained (a bare rename leaves `-wal`/`-shm` sidecars behind). The
    // import itself reads THROUGH the WAL either way; this keeps the `.imported` artifact whole.
    checkpoint_source_wal(&source);

    // Open the legacy DB READ-ONLY and copy the authored + expensive rows across in one
    // transaction.
    let source_storage = IndexConnection::open_read_only_blocking(&source)
        .with_context(|| format!("opening the legacy index {} read-only", source.display()))?;
    let counts = import_from_source(source_storage.connection(), target_conn, &repo_id)?;
    drop(source_storage);

    // Author the freshly-imported (remapped) rows into the TARGET's owner stream so the
    // consolidated store's signed history is complete under its OWN device identity (#541). The
    // per-chain backfill gate would otherwise skip them (the target chain is already
    // non-empty), and a later update/obsolete on an imported memory would author an inert op.
    // The source's pre-remap signed entries are deliberately NOT carried — their signatures
    // cover the source identity + pre-remap ids. Runs BEFORE the rename, so a failure leaves
    // the legacy file in place to retry.
    //
    // KNOWN retry-window gap (decision 8): a re-run's presence-only reconcile does NOT re-author a
    // CONTENT edit made in the window between a committed import and a failed rename, nor tombstone
    // a window edge REMOVAL (a phantom projected edge). Both fall under the same out-of-scope
    // class as raw out-of-band content divergence; they are content/tombstone divergence, not the
    // missing-NodeCreate bug #541 fixes, and the log is a shadow until phase D.
    crate::query::memory::reconcile_owner_stream_for_repo(target_conn, &repo_id, schema::now_ms())?;

    // Rename the legacy file so a keyless config now resolves to the global store (via the
    // `.imported` latch), and a re-run is a no-op. AFTER the import commits, so a failure leaves
    // the legacy file in place to retry. The WAL sidecars travel WITH the archive: a bare
    // main-file rename would orphan `-wal`/`-shm` as permanent litter, and any un-checkpointed
    // frames in the WAL belong to the archive (SQLite opens the renamed pair as a unit) — the
    // same discipline the custom-pin remedy tells users to follow.
    fs::rename(&source, &imported)
        .with_context(|| format!("renaming {} to {}", source.display(), imported.display()))?;
    rename_wal_sidecars(&source, &imported);

    Ok(ConsolidateOutcome::Imported(ImportSummary {
        repo_id,
        source,
        renamed_to: imported,
        target,
        memories: counts.memories,
        bindings: counts.bindings,
        tags: counts.tags,
        call_paths: counts.call_paths,
        call_path_edges: counts.call_path_edges,
        edges: counts.edges,
        embedding_cache_rows: counts.embedding_cache_rows,
        meta_keys: counts.meta_keys,
    }))
}

/// The real (non-placeholder) repo ids the SOURCE legacy DB's own `repos` registry records — the
/// ids pre-transition writers key their legacy-side flocks by (a pre-deepen watcher holds the old
/// `local:` id's lock). Read-only, pre-lock, and TOLERANT: any failure (a source predating the
/// V038 registry, an unreadable file) peeks empty — such vintages were only ever written by
/// pre-per-repo-lock binaries, which no current lock scheme can coordinate with anyway.
fn source_registered_repo_ids(source: &Path) -> Vec<String> {
    let Ok(storage) = IndexConnection::open_read_only_blocking(source) else {
        return Vec::new();
    };
    let conn = storage.connection();
    let Ok(mut stmt) = conn.prepare("SELECT repo_id FROM repos WHERE repo_id != '__unassigned__'")
    else {
        return Vec::new();
    };
    stmt.query_map([], |row| row.get::<_, String>(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// `<source>.imported` — the marker the legacy file is renamed to after a successful import.
fn imported_marker(source: &Path) -> PathBuf {
    let mut name = source.as_os_str().to_os_string();
    name.push(".imported");
    PathBuf::from(name)
}

/// Resolve the identity consolidate registers and locks under. A non-git root with no pinned
/// `[index] repo_id` has no derivable identity to scope the import under, so consolidation refuses
/// it with an actionable message rather than guess.
fn resolve_identity_for_config(config: &Config) -> anyhow::Result<RepoIdentity> {
    match repo_identity::resolve_repo_identity(&config.root, config.repo_id_override.as_deref()) {
        Ok(identity) => Ok(identity),
        Err(err) if err.is_absent() => anyhow::bail!(
            "cannot determine a repo identity to consolidate {}: it is not a git repository and \
             rag-rat.toml pins no `[index] repo_id`. Pin `[index] repo_id = \"…\"` to consolidate \
             a non-git root.",
            config.root.display(),
        ),
        Err(err) => Err(err.into()),
    }
}

/// The pinned-`database` refusal, shaped by WHERE the pin points. A pin at the DEFAULT legacy path
/// needs only the key removed — the keyless re-run resolves straight to the file. A CUSTOM pin
/// additionally needs its file MOVED to the default location first: keyless resolution never looks
/// at a custom path, so removing the key alone would leave the custom index invisible (a follow-up
/// run reports `no_legacy_index` / `already_global`) and its memories never imported. The custom
/// remedy prints the literal commands for the user's paths.
fn pinned_refusal_message(source: &Path, default_legacy: &Path) -> String {
    let base = format!(
        "refusing to consolidate: rag-rat.toml pins `[index] database`, so this repo would keep \
         using {} and any memories written there after an import would be silently lost when the \
         file is later renamed.",
        source.display(),
    );
    if source == default_legacy {
        format!(
            "{base} Remove the `database` key from rag-rat.toml, then re-run `rag-rat \
             consolidate` — the single completing run imports and renames with no divergence \
             window."
        )
    } else {
        format!(
            "{base} The pin points at a CUSTOM path, which a keyless config never consults — \
             removing the key alone would leave this index invisible and its memories unimported. \
             Move it to the default location, remove the `database` key from rag-rat.toml, then \
             re-run `rag-rat consolidate`:\n    mkdir -p {default_dir} && mv {src} \
             {default}\n(move {src}-wal / {src}-shm alongside if present — they can hold recent \
             writes)",
            default_dir = default_legacy.parent().unwrap_or(Path::new(".")).display(),
            src = source.display(),
            default = default_legacy.display(),
        )
    }
}

/// Acquire the per-repo write lock beside `database` for `repo_id`, bounded — `side` names which
/// file ("global" / "legacy") in the refusal so a timeout is actionable.
fn acquire_consolidate_lock(
    database: &Path,
    repo_id: &str,
    side: &str,
) -> anyhow::Result<locks::WriteLock> {
    locks::WriteLock::acquire_timeout(database, repo_id, CONSOLIDATE_LOCK_TIMEOUT)?.ok_or_else(
        || {
            anyhow::anyhow!(
                "timed out waiting for an in-flight writer holding this repo's {side}-side write \
                 lock (an index or maintenance pass); re-run `rag-rat consolidate` once it \
                 finishes"
            )
        },
    )
}

/// Move the legacy DB's `-wal` / `-shm` sidecars beside the renamed `.imported` archive, so no
/// litter remains at the legacy path and any un-checkpointed WAL frames TRAVEL with the archive
/// (SQLite opens the main+wal pair as a unit — the archive stays whole even when the best-effort
/// checkpoint was refused by a concurrent reader). Best-effort per file: the main rename already
/// committed the consolidation, so a sidecar rename failure is a warn — the LIVE import read
/// through the WAL and lost nothing; only the archive may lag its sidecar.
fn rename_wal_sidecars(source: &Path, imported: &Path) {
    for suffix in ["-wal", "-shm"] {
        let sidecar = path_with_suffix(source, suffix);
        if !sidecar.exists() {
            continue;
        }
        let dest = path_with_suffix(imported, suffix);
        if let Err(err) = fs::rename(&sidecar, &dest) {
            tracing::warn!(
                sidecar = %sidecar.display(),
                "failed to move a legacy WAL sidecar beside the .imported archive: {err}"
            );
        }
    }
}

/// `path` with `suffix` appended to its final component (`index.sqlite` + `-wal` →
/// `index.sqlite-wal`) — how SQLite names its WAL sidecars.
fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Best-effort `PRAGMA wal_checkpoint(TRUNCATE)` on the legacy file, folding its WAL into the
/// main file before the snapshot read / rename when nothing contends. DELIBERATELY not fatal on a
/// busy checkpoint: the lockless read-only MCP openers are a SANCTIONED reader class (they take no
/// per-repo flock by design), so a reader holding the WAL open here is a legitimate state, not a
/// lock-discipline violation — and correctness does not depend on the checkpoint succeeding: the
/// import reads through the WAL on its own connection, and [`rename_wal_sidecars`] moves any
/// un-checkpointed frames WITH the archive.
fn checkpoint_source_wal(source: &Path) {
    let checkpoint = Connection::open(source).and_then(|conn| {
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get::<_, i64>(0))
    });
    match checkpoint {
        Ok(0) => {},
        Ok(_busy) => tracing::warn!(
            path = %source.display(),
            "could not fully checkpoint the legacy index's WAL (a reader is holding it open); the \
             import is unaffected, but the renamed .imported file may lag its -wal sidecar"
        ),
        Err(err) => tracing::warn!(
            path = %source.display(),
            "failed to checkpoint the legacy index's WAL before import: {err}"
        ),
    }
}

/// Import counts, threaded back to the [`ImportSummary`].
struct ImportCounts {
    memories: u64,
    bindings: u64,
    tags: u64,
    call_paths: u64,
    call_path_edges: u64,
    edges: u64,
    embedding_cache_rows: u64,
    meta_keys: u64,
}

/// Copy every authored + expensive row from `source` into `target` under `repo_id`, in ONE
/// IMMEDIATE transaction (all-or-nothing; the SQLite write lock is taken up front instead of on a
/// mid-transaction upgrade). Every SOURCE table is guarded by presence so a legacy DB predating a
/// feature (e.g. `embedding_cache`, added later than the memory tables) is imported for what it
/// does have rather than erroring.
///
/// THE MIRROR INVARIANT (Codex batch 9, closes the retry-window class): until the archive rename
/// lands, the legacy DB is AUTHORITATIVE for this repo's imported slice — keyless resolution
/// keeps serving the legacy file, so any edit in the window between a committed import and a
/// failed rename happens THERE. Every artifact the import copies is therefore REFRESHED to match
/// the source on every run; after the rename, the `.imported` latch makes re-runs unreachable.
/// Per-artifact disposition (every copied artifact MUST appear here and obey the invariant —
/// a new artifact gets classified at birth):
///  * `repo_memories` (parents)          — content-gated UPSERT ([`copy_memories`]); a no-edit
///    retry writes nothing, foreign rows are never updated (ownership rides the gate).
///  * children of ALL mapped ids         — REPLACED unconditionally ([`refresh_children`]): the
///    (tags/bindings/call-paths/edges)     children of every mapped target id — same-repo AND
///    remapped — are deleted, then reinserted from the source. Unconditional because a parent-edit
///    gate needs a "children changed ⇒ parent row changed" signal (updated_at_ms chaining) that is
///    true today but brittle — the batch-8 gate already missed remapped parents. Replace-in-txn is
///    convergent and signal-free. Counts stay honest via a before/after slice DIGEST per table:
///    identical slice ⇒ 0, else the reinserted rows.
///  * `repo_memory_fts`                  — re-derived for the WHOLE repo at the end
///    ([`rebuild_memory_fts_for_repo`]); covers refreshed same-repo AND remapped rows alike (both
///    are stamped `repo_id` = ours by the copy).
///  * `repo_meta` model-state unit       — per-key MIRROR ([`copy_model_state`]): value-gated
///    upsert for keys present in the source, DELETE for carried keys absent there (a model switch
///    in the window may legitimately remove a key, e.g. the remote config when moving to a local
///    model — keeping it would tear the unit).
///  * `ai_models` readiness              — restore-style carry ([`carry_active_model_readiness`]),
///    re-derived from the SOURCE's active model each run, so a window model change restores the NEW
///    model's readiness on retry; an explicit machine-level `disabled` is never overridden.
///  * `embedding_cache`                  — `INSERT OR IGNORE`, the ONE legitimate IGNORE: rows are
///    CONTENT-ADDRESSED (`(input_hash, model_id)` determines the vector bytes), so an existing row
///    is by definition identical and a "stale" extra row is harmless cache that re-embedding never
///    consults incorrectly.
///
/// Ends by re-deriving the `repo_memory_fts` mirror for the repo: the copies write the base
/// tables directly, and `memory_search` retrieves EXCLUSIVELY through the FTS mirror — without
/// this, imported memories would be permanently invisible to keyword search (no reconcile/index
/// path repairs the mirror).
fn import_from_source(
    source: &Connection,
    target: &Connection,
    repo_id: &str,
) -> anyhow::Result<ImportCounts> {
    let tx =
        rusqlite::Transaction::new_unchecked(target, rusqlite::TransactionBehavior::Immediate)?;
    // CHILD-OWNERSHIP INVARIANT: `copy_memories` returns the source→target MEMORY-ID MAP, and every
    // child copy inserts ONLY under a mapped target id — an id this import verified it owns under
    // `repo_id` this run. A child row must never attach to a parent the import does not own: a
    // legacy memory id colliding with ANOTHER repo's memory in the global store would otherwise
    // have its memory dropped while its tags/bindings/call-paths silently contaminate the other
    // repo's memory.
    let CopiedMemories { rows_written: memories, id_map } = copy_memories(source, &tx, repo_id)?;
    // Mirror invariant: children of EVERY mapped id are replaced, not unioned. Digest the target's
    // child slices before and after — identical slice ⇒ that table reports 0 (an honest no-op).
    let pre = child_slice_digests(&tx, &id_map)?;
    refresh_children(&tx, &id_map)?;
    let raw = ImportCounts {
        memories,
        bindings: copy_bindings(source, &tx, repo_id, &id_map)?,
        tags: copy_tags(source, &tx, &id_map)?,
        call_paths: copy_call_paths(source, &tx, &id_map)?,
        call_path_edges: copy_call_path_edges(source, &tx, &id_map)?,
        edges: copy_node_edges(source, &tx, repo_id, &id_map)?,
        embedding_cache_rows: copy_embedding_cache(source, &tx)?,
        meta_keys: copy_model_state(source, &tx, repo_id)?,
    };
    let post = child_slice_digests(&tx, &id_map)?;
    let counts = ImportCounts {
        bindings: if pre.bindings == post.bindings { 0 } else { raw.bindings },
        tags: if pre.tags == post.tags { 0 } else { raw.tags },
        call_paths: if pre.call_paths == post.call_paths { 0 } else { raw.call_paths },
        call_path_edges: if pre.call_path_edges == post.call_path_edges {
            0
        } else {
            raw.call_path_edges
        },
        edges: if pre.edges == post.edges { 0 } else { raw.edges },
        ..raw
    };
    rebuild_memory_fts_for_repo(&tx, repo_id)?;
    tx.commit()?;
    Ok(counts)
}

/// One SHA-256 digest per child table over the TARGET rows of the mapped ids (order-insensitive:
/// rows are serialized with type tags and sorted before hashing). Drives the honest-count gate in
/// [`import_from_source`]: replace-then-reinsert genuinely rewrites rows on every run, but a run
/// that leaves a slice byte-identical did no work worth reporting.
struct ChildSliceDigests {
    tags: [u8; 32],
    bindings: [u8; 32],
    call_paths: [u8; 32],
    call_path_edges: [u8; 32],
    edges: [u8; 32],
}

fn child_slice_digests(
    tx: &Connection,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<ChildSliceDigests> {
    Ok(ChildSliceDigests {
        tags: child_slice_digest(tx, "repo_memory_tags", "memory_id", id_map)?,
        bindings: child_slice_digest(tx, "repo_memory_bindings", "memory_id", id_map)?,
        call_paths: child_slice_digest(tx, "repo_memory_call_paths", "memory_id", id_map)?,
        call_path_edges: child_slice_digest(
            tx,
            "repo_memory_call_path_edges",
            "memory_id",
            id_map,
        )?,
        // Node edges key on `source_node_id` (the owning node), not `memory_id`.
        edges: child_slice_digest(tx, "repo_node_edges", "source_node_id", id_map)?,
    })
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
                        line.push_str(
                            &v.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
                        );
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
fn copy_memories(
    source: &Connection,
    tx: &Connection,
    repo_id: &str,
) -> anyhow::Result<CopiedMemories> {
    if !schema::table_exists(source, "repo_memories")? {
        return Ok(CopiedMemories::default());
    }
    let mut stmt = source.prepare(
        "SELECT id, kind, title, body, confidence, status, created_by, created_at_ms, \
         updated_at_ms, source, source_text_hash, input_hash, memory_version, payload_json FROM \
         repo_memories",
    )?;
    let mut rows = stmt.query([])?;
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
fn refresh_children(tx: &Connection, id_map: &BTreeMap<String, String>) -> anyhow::Result<()> {
    for id in id_map.values() {
        for table in [
            "repo_memory_tags",
            "repo_memory_bindings",
            "repo_memory_call_paths",
            "repo_memory_call_path_edges",
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE memory_id = ?1"), [id])?;
        }
        // Node edges (#464) key on `source_node_id`, not `memory_id` — delete them here too so the
        // subsequent `copy_node_edges` REPLACES the source's edge set (the mirror invariant).
        tx.execute("DELETE FROM repo_node_edges WHERE source_node_id = ?1", [id])?;
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
    let hex: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    format!("mem_{}_{}", &hex[..13], &hex[13..25])
}

/// Copy `repo_memory_bindings`, stamping `repo_id` and NULLing ONLY the LOCAL rowid columns
/// (`logical_symbol_id` / `symbol_id` / `chunk_id` / `edge_id`) so the validate loop re-resolves
/// them from the portable anchor after the next index pass (spec §4.5). EVERY portable column is
/// copied verbatim — including the relocation-provenance set (`symbol_kind`, `signature_hash`,
/// `moniker_tool`, `moniker_tool_version`, `relocation_reason`): moniker validation reports
/// `unverified` without `moniker_tool`, and moniker relocation requires both tool fields, so
/// dropping them would permanently strip imported `scip_moniker` bindings of their oracle-backed
/// relocation path. The column probes tolerate a legacy DB predating the signals /
/// moniker-provenance migrations (absent columns import as NULL).
fn copy_bindings(
    source: &Connection,
    tx: &Connection,
    repo_id: &str,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_memory_bindings")? {
        return Ok(0);
    }
    let symbol_kind = source_column_or_null(source, "symbol_kind")?;
    let signature_hash = source_column_or_null(source, "signature_hash")?;
    let moniker_tool = source_column_or_null(source, "moniker_tool")?;
    let moniker_tool_version = source_column_or_null(source, "moniker_tool_version")?;
    let relocation_reason = source_column_or_null(source, "relocation_reason")?;
    // The tracker columns exist per source VINTAGE: a post-V060 source carries
    // tracker/project/item_key, a pre-V060 source carries github_owner/github_repo/github_number
    // — probe both shapes and convert legacy `github` bindings to the `tracker` kind below (the
    // V060 mapping, applied at the import seam because a foreign source file is read as-is,
    // never migrated).
    let tracker_col = source_column_or_null(source, "tracker")?;
    let project_col = source_column_or_null(source, "project")?;
    let item_key_col = source_column_or_null(source, "item_key")?;
    let github_owner = source_column_or_null(source, "github_owner")?;
    let github_repo = source_column_or_null(source, "github_repo")?;
    let github_number = source_column_or_null(source, "github_number")?;
    let mut stmt = source.prepare(&format!(
        "SELECT memory_id, binding_kind, binding_id, path, start_line, end_line, commit_hash, \
         {tracker_col}, {project_col}, {item_key_col}, {github_owner}, {github_repo}, \
         {github_number}, anchor_status, created_at_ms, {symbol_kind}, {signature_hash}, \
         {moniker_tool}, {moniker_tool_version}, {relocation_reason}
         FROM repo_memory_bindings",
    ))?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        // Only rows whose parent memory this import OWNS (the id map); an unmapped memory_id is a
        // dangling orphan in the source — dropped, never attached to a stranger's memory.
        let Some(memory_id) = id_map.get(&row.get::<_, String>(0)?) else {
            continue;
        };
        let mut binding_kind = row.get::<_, String>(1)?;
        let mut binding_id = row.get::<_, String>(2)?;
        let mut tracker = row.get::<_, Option<String>>(7)?;
        let mut project = row.get::<_, Option<String>>(8)?;
        let mut item_key = row.get::<_, Option<String>>(9)?;
        // Legacy `github` bindings convert to the `tracker` kind — exactly the V060 backfill
        // mapping, so an imported binding is indistinguishable from a migrated one.
        if binding_kind == "github"
            && let (Some(owner), Some(gh_repo), Some(number)) = (
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<i64>>(12)?,
            )
        {
            binding_kind = "tracker".to_string();
            binding_id = format!("github:{owner}/{gh_repo}#{number}");
            tracker = Some("github".to_string());
            project = Some(format!("{owner}/{gh_repo}"));
            item_key = Some(number.to_string());
        }
        let changed = tx.execute(
            "INSERT OR IGNORE INTO repo_memory_bindings(memory_id, binding_kind, binding_id, \
             path, start_line, end_line, logical_symbol_id, symbol_id, chunk_id, edge_id, \
             commit_hash, tracker, project, item_key, anchor_status, created_at_ms, symbol_kind, \
             signature_hash, moniker_tool, moniker_tool_version, relocation_reason, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, NULL, ?7, ?8, ?9, ?10, ?11, ?12, \
             ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                memory_id,
                binding_kind,
                binding_id,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
                tracker,
                project,
                item_key,
                row.get::<_, String>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, Option<String>>(15)?,
                row.get::<_, Option<String>>(16)?,
                row.get::<_, Option<String>>(17)?,
                row.get::<_, Option<String>>(18)?,
                row.get::<_, Option<String>>(19)?,
                repo_id,
            ],
        )?;
        count += changed as u64;
    }
    Ok(count)
}

/// `column` when the SOURCE `repo_memory_bindings` carries it, else a `NULL AS column` literal —
/// the probe that lets [`copy_bindings`] read one SELECT shape from any legacy vintage. `column`
/// is a compile-time constant at every call site, never user input.
fn source_column_or_null(source: &Connection, column: &str) -> anyhow::Result<String> {
    Ok(if schema::column_exists(source, "repo_memory_bindings", column)? {
        column.to_string()
    } else {
        format!("NULL AS {column}")
    })
}

/// Copy `repo_memory_tags` (scoped transitively via `memory_id` — no `repo_id` column; both
/// columns copied, the full table shape).
/// Copy `repo_node_edges` (#464), stamping the OWNER `repo_id` and REMAPPING both endpoints through
/// the id map. An edge's SOURCE must map — an edge of an unmapped memory is a dangling orphan in
/// the source, dropped, never attached to a stranger (the child-ownership invariant). A NODE target
/// that ALSO maps is remapped (id + repo) and `current`; a node target that does NOT map is kept
/// verbatim as an `unresolved` cross-repo reference; a github target re-homes to the import repo
/// and stays `current`. The `edge_key` is RECOMPUTED from the remapped coordinates — it
/// content-addresses owner+source+target, all of which change on import. Local rowid columns are
/// NOT copied (re-resolved on read); `INSERT OR IGNORE` because `refresh_children` cleared the
/// source's edge set this run.
fn copy_node_edges(
    source: &Connection,
    tx: &Connection,
    repo_id: &str,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_node_edges")? {
        return Ok(0);
    }
    let mut stmt = source.prepare(
        "SELECT source_node_id, relation, target_repo_id, target_kind, target_anchor, \
         created_at_ms FROM repo_node_edges",
    )?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        // Child-ownership: only edges whose SOURCE this import owns; an unmapped source is dropped.
        let Some(source_node_id) = id_map.get(&row.get::<_, String>(0)?) else {
            continue;
        };
        let relation = row.get::<_, String>(1)?;
        let src_target_repo = row.get::<_, String>(2)?;
        let target_kind = row.get::<_, String>(3)?;
        let src_target_anchor = row.get::<_, String>(4)?;
        let created_at_ms = row.get::<_, i64>(5)?;
        let (target_repo_id, target_anchor, target_node_id, anchor_status) =
            match target_kind.as_str() {
                "node" => match id_map.get(&src_target_anchor) {
                    Some(mapped) =>
                        (repo_id.to_string(), mapped.clone(), Some(mapped.clone()), "current"),
                    None => (src_target_repo, src_target_anchor.clone(), None, "unresolved"),
                },
                _ => (repo_id.to_string(), src_target_anchor.clone(), None, "current"),
            };
        let key =
            crate::query::memory::edge_key(source_node_id, &relation, &target_kind, &target_anchor);
        let changed = tx.execute(
            "INSERT OR IGNORE INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, \
             target_repo_id, target_kind, target_anchor, target_node_id, \
             target_logical_symbol_id, symbol_kind, signature_hash, anchor_status, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL, ?9, ?10)",
            params![
                key,
                repo_id,
                source_node_id,
                relation,
                target_repo_id,
                target_kind,
                target_anchor,
                target_node_id,
                anchor_status,
                created_at_ms
            ],
        )?;
        count += changed as u64;
    }
    Ok(count)
}

fn copy_tags(
    source: &Connection,
    tx: &Connection,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_memory_tags")? {
        return Ok(0);
    }
    let mut stmt = source.prepare("SELECT memory_id, tag FROM repo_memory_tags")?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let Some(memory_id) = id_map.get(&row.get::<_, String>(0)?) else {
            continue;
        };
        let changed = tx.execute(
            "INSERT OR IGNORE INTO repo_memory_tags(memory_id, tag) VALUES (?1, ?2)",
            params![memory_id, row.get::<_, String>(1)?],
        )?;
        count += changed as u64;
    }
    Ok(count)
}

/// Copy `repo_memory_call_paths`, NULLing the local `start`/`end_logical_symbol_id` (re-resolved
/// by the validate loop, like the bindings' rowid columns); the portable identity
/// (`edge_sequence_hash`, `path_summary`, `created_at_ms`) is copied verbatim.
fn copy_call_paths(
    source: &Connection,
    tx: &Connection,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_memory_call_paths")? {
        return Ok(0);
    }
    let mut stmt = source.prepare(
        "SELECT memory_id, edge_sequence_hash, path_summary, created_at_ms
         FROM repo_memory_call_paths",
    )?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let Some(memory_id) = id_map.get(&row.get::<_, String>(0)?) else {
            continue;
        };
        let changed = tx.execute(
            "INSERT OR IGNORE INTO repo_memory_call_paths(memory_id, start_logical_symbol_id, \
             end_logical_symbol_id, edge_sequence_hash, path_summary, created_at_ms)
             VALUES (?1, NULL, NULL, ?2, ?3, ?4)",
            params![
                memory_id,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ],
        )?;
        count += changed as u64;
    }
    Ok(count)
}

/// Copy `repo_memory_call_path_edges` verbatim — every column is a row-id-independent portable
/// identity used to re-find the edge (the full 9-column table shape), so nothing is nulled.
fn copy_call_path_edges(
    source: &Connection,
    tx: &Connection,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_memory_call_path_edges")? {
        return Ok(0);
    }
    let mut stmt = source.prepare(
        "SELECT memory_id, edge_sequence_hash, ordinal, edge_fingerprint, from_name, to_name, \
         edge_kind, target_qualified_name, receiver_hint FROM repo_memory_call_path_edges",
    )?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let Some(memory_id) = id_map.get(&row.get::<_, String>(0)?) else {
            continue;
        };
        let changed = tx.execute(
            "INSERT OR IGNORE INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, \
             ordinal, edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name, \
             receiver_hint)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                memory_id,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ],
        )?;
        count += changed as u64;
    }
    Ok(count)
}

/// Copy `embedding_cache` — content-addressed by `input_hash` (which folds model + version + input
/// text), so `INSERT OR IGNORE` is a conflict-free union — the ONE copy the mirror invariant
/// exempts, because rows are CONTENT-ADDRESSED: `(input_hash, model_id)` determines the vector
/// bytes, an existing row is by definition identical (same content) and an extra unreferenced row
/// is harmless cache. A vector already present (same content)
/// is kept, a new one is added (the full 6-column table shape). This is the durable unit that
/// makes re-embedding the consolidated repo a no-op. It is a GLOBAL/shared table (no `repo_id`).
fn copy_embedding_cache(source: &Connection, tx: &Connection) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "embedding_cache")? {
        return Ok(0);
    }
    let mut stmt = source.prepare(
        "SELECT input_hash, model_id, embedding_dim, vector_blob, computed_at_ms, last_used_at_ms
         FROM embedding_cache",
    )?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let changed = tx.execute(
            "INSERT OR IGNORE INTO embedding_cache(input_hash, model_id, embedding_dim, \
             vector_blob, computed_at_ms, last_used_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ],
        )?;
        count += changed as u64;
    }
    Ok(count)
}

/// Carry the repo's MODEL STATE as one coherent unit: the portable `repo_meta` keys
/// ([`CARRIED_META_KEYS`] — identity, freshness version, remote-endpoint config, and the
/// provisional-provenance flag) plus the active model's `ai_models` READINESS row. Splitting this
/// across the move breaks it as a set: the cache rows are useless without the model identity, the
/// identity routes nowhere without the remote config, an absent provisional flag hardens an
/// auto-pick into a config-immune choice, and an identity pointing at a `MissingModel` row makes
/// `active_embedder` refuse — semantic search/reconcile "not ready" despite the carried cache
/// making re-embedding a no-op. Each `repo_meta` carry MIRRORS the source per key (see
/// [`CARRIED_META_KEYS`]): value-gated upsert when present, delete when absent — counts reflect
/// rows actually changed, so a no-edit retry reports zero. The legacy DB is single-repo, so
/// values are read by KEY regardless of the source's own `repo_id`.
fn copy_model_state(source: &Connection, tx: &Connection, repo_id: &str) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_meta")? {
        return Ok(0);
    }
    let mut count = 0u64;
    let mut active_model: Option<String> = None;
    for key in CARRIED_META_KEYS {
        let value: Option<String> = source
            .query_row("SELECT value FROM repo_meta WHERE key = ?1 LIMIT 1", [key], |row| {
                row.get(0)
            })
            .optional()?
            .flatten();
        let changed = match value {
            Some(value) => {
                if *key == "active_embedding_model" {
                    active_model = Some(value.clone());
                }
                tx.execute(
                    "INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, ?3)
                     ON CONFLICT(repo_id, key) DO UPDATE SET value = excluded.value
                     WHERE repo_meta.value IS NOT excluded.value",
                    params![repo_id, key, value],
                )?
            },
            // Absent in the authoritative source: a window model switch may have REMOVED the key
            // (absence has meaning — batch 6); a surviving stale copy would tear the unit.
            None => tx
                .execute("DELETE FROM repo_meta WHERE repo_id = ?1 AND key = ?2", params![
                    repo_id, key
                ])?,
        };
        count += changed as u64;
    }
    if let Some(model_id) = active_model {
        carry_active_model_readiness(source, tx, &model_id)?;
    }
    Ok(count)
}

/// Carry the active model's `ai_models` READINESS onto the target when the legacy DB holds it
/// Ready and the target does not. WHY carrying `Ready` is sound here: consolidation is
/// SAME-MACHINE by construction, and `Ready` asserts machine-level availability — fastembed
/// artifacts live in the machine-global HF cache (which `recover_cached_fastembed_model` re-probes
/// on scoped opens, so a stale carry self-corrects), remote runtimes reconstruct their transport
/// from the carried `active_embedding_remote_config` at use time, and the hash model needs no
/// artifacts at all. A misjudged carry surfaces as a use-time embed error and is repaired by
/// install/recovery — never data corruption. GUARD: a target row with `disabled = 1` is an
/// explicit machine-level opt-out shared by every repo in the global DB — never overridden.
fn carry_active_model_readiness(
    source: &Connection,
    tx: &Connection,
    model_id: &str,
) -> anyhow::Result<()> {
    if !schema::table_exists(source, "ai_models")? {
        return Ok(());
    }
    // Only a legacy row that is genuinely Ready (installed, not disabled) is worth carrying.
    let legacy: Option<(Option<i64>, String, Option<i64>)> = source
        .query_row(
            "SELECT embedding_dim, runtime, installed_at_ms FROM ai_models
             WHERE model_id = ?1 AND installed = 1 AND disabled = 0 AND status = 'Ready'",
            [model_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((embedding_dim, runtime, installed_at_ms)) = legacy else {
        return Ok(());
    };
    // Absent on the target → seed the full row Ready.
    let changed = tx.execute(
        "INSERT OR IGNORE INTO ai_models(model_id, capability, embedding_dim, runtime, installed, \
         disabled, status, installed_at_ms, last_error)
         VALUES (?1, 'embedding', ?2, ?3, 1, 0, 'Ready', ?4, NULL)",
        params![model_id, embedding_dim, runtime, installed_at_ms],
    )?;
    if changed > 0 {
        return Ok(());
    }
    // Present but not usable (e.g. the manifest seeded it `MissingModel`) → restore the legacy
    // readiness, UNLESS explicitly disabled on the target (machine-level opt-out wins).
    tx.execute(
        "UPDATE ai_models
         SET installed = 1, status = 'Ready', embedding_dim = ?2, runtime = ?3,
             installed_at_ms = ?4, last_error = NULL
         WHERE model_id = ?1 AND disabled = 0 AND NOT (installed = 1 AND status = 'Ready')",
        params![model_id, embedding_dim, runtime, installed_at_ms],
    )?;
    Ok(())
}

/// Re-derive the `repo_memory_fts` mirror for `repo_id` from the freshly-imported base tables —
/// the V042-rebuild shape (same space-joined tag derivation as `upsert_memory_fts`). Runs inside
/// the import transaction. Delete-then-insert scoped to the repo keeps a retry convergent (the
/// mirror has no PK, so re-inserting would otherwise accumulate duplicate rows) and re-derives any
/// pre-existing global-side memories of this repo to identical content.
fn rebuild_memory_fts_for_repo(tx: &Connection, repo_id: &str) -> anyhow::Result<()> {
    tx.execute("DELETE FROM repo_memory_fts WHERE repo_id = ?1", [repo_id])?;
    tx.execute(
        "INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
         SELECT
             m.repo_id, m.id, m.title, m.body, m.kind,
             COALESCE(
                 (SELECT group_concat(t.tag, ' ')
                  FROM repo_memory_tags t WHERE t.memory_id = m.id),
                 ''
             )
         FROM repo_memories m
         WHERE m.repo_id = ?1",
        [repo_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    /// A source legacy DB (current schema) seeded with 3 memories — one carrying a binding whose
    /// LOCAL rowid columns are set alongside FULL relocation provenance (symbol_kind /
    /// signature_hash / moniker trio), a tag, a call-path + edge — plus 2 embedding-cache rows and
    /// the model-identity meta keys.
    fn seeded_source() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        for (id, title) in [("m1", "one"), ("m2", "two"), ("m3", "three")] {
            conn.execute(
                "INSERT INTO repo_memories(id, kind, title, body, confidence, status, \
                 created_at_ms, updated_at_ms, source, memory_version, repo_id)
                 VALUES (?1, 'Invariant', ?2, 'body', 'high', 'active', 0, 0, 'agent', 'v1', \
                 'legacy-repo')",
                params![id, title],
            )
            .unwrap();
        }
        // #465: m1 carries a polymorphic payload — it must survive import verbatim, not be NULLed.
        conn.execute(
            r#"UPDATE repo_memories SET payload_json = '{"priority":1}' WHERE id = 'm1'"#,
            [],
        )
        .unwrap();
        // A binding on m1 with the LOCAL rowid columns populated (must be NULLed on import) and
        // EVERY portable field set (must survive verbatim), including the moniker relocation
        // provenance.
        conn.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
             start_line, end_line, logical_symbol_id, symbol_id, chunk_id, edge_id, commit_hash, \
             tracker, project, item_key, anchor_status, created_at_ms, symbol_kind, \
             signature_hash, moniker_tool, moniker_tool_version, relocation_reason, repo_id)
             VALUES ('m1', 'path', 'b1', 'src/x.rs', 10, 20, 111, 222, 333, 444, 'abc', 'github', \
             'o/r', '7', 'current', 0, 'function', 'sighash', 'scip-rust', '0.4', 'moved', \
             'legacy-repo')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES ('m1', 'tagalpha')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO repo_memory_call_paths(memory_id, start_logical_symbol_id, \
             end_logical_symbol_id, edge_sequence_hash, path_summary, created_at_ms)
             VALUES ('m1', 555, 666, 'h1', 'a -> b', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, ordinal, \
             edge_fingerprint, to_name, edge_kind) VALUES ('m1', 'h1', 0, 'fp', 'b', 'calls')",
            [],
        )
        .unwrap();
        // #464: a node edge m1 --depends_on--> m2. Its `edge_key` is RECOMPUTED on import from the
        // (possibly remapped) endpoints, so the seed's placeholder key here is intentionally not
        // the real one.
        // Three edge shapes exercise every `copy_node_edges` branch on import: a node target that
        // IS carried (remapped, current), a github target (re-homed to the import repo, current),
        // and a node target that is NOT carried (kept as an `unresolved` cross-repo
        // reference).
        for (key, relation, target_repo, kind, anchor, node, status) in [
            ("seed-node", "depends_on", "legacy-repo", "node", "m2", "m2", "current"),
            ("seed-gh", "tracks", "legacy-repo", "github", "o/r#7", "", "current"),
            ("seed-ext", "relates_to", "other-repo", "node", "external-node", "", "unresolved"),
        ] {
            let node_id: Option<&str> = (!node.is_empty()).then_some(node);
            conn.execute(
                "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, \
                 target_repo_id, target_kind, target_anchor, target_node_id, anchor_status, \
                 created_at_ms) VALUES (?1, 'legacy-repo', 'm1', ?2, ?3, ?4, ?5, ?6, ?7, 0)",
                rusqlite::params![key, relation, target_repo, kind, anchor, node_id, status],
            )
            .unwrap();
        }
        for (hash, dim) in [("ih1", 384), ("ih2", 768)] {
            conn.execute(
                "INSERT INTO embedding_cache(input_hash, model_id, embedding_dim, vector_blob, \
                 computed_at_ms, last_used_at_ms) VALUES (?1, 'model-a', ?2, X'00', 0, 0)",
                params![hash, dim],
            )
            .unwrap();
        }
        // Seed the meta under the placeholder (which always exists in `repos`, so the FK holds);
        // `copy_model_state` reads it by KEY regardless of the source's repo_id. Keys from both
        // classification classes: three PORTABLE keys (identity, remote config, and the
        // provisional flag — an auto-picked model, "1") and one DB-LOCAL freshness cursor (must
        // NOT copy).
        for (key, value) in [
            ("active_embedding_model", "model-a"),
            ("active_embedding_remote_config", "{\"endpoint\":\"http://ollama:11434\"}"),
            ("active_embedding_model_provisional", "1"),
            ("git_commit", "cursor-sha"),
        ] {
            conn.execute(
                "INSERT INTO repo_meta(repo_id, key, value) VALUES ('__unassigned__', ?1, ?2)",
                params![key, value],
            )
            .unwrap();
        }
        // The active model's READINESS row — part of the model-state unit `copy_model_state`
        // carries (an identity pointing at a MissingModel/absent row leaves active_embedder
        // refusing despite the carried cache).
        conn.execute(
            "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, \
             disabled, status, installed_at_ms) VALUES ('model-a', 'embedding', 384, 'local', 1, \
             0, 'Ready', 7)",
            [],
        )
        .unwrap();
        conn
    }

    fn fresh_target() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        // The target must hold the repo `import_from_source` stamps: `repo_meta.repo_id` has a FK
        // to `repos` (in production `register_repo` creates this row before the import runs).
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('global-repo', \
             'g', 0)",
            [],
        )
        .unwrap();
        conn
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn import_stamps_the_new_repo_id_and_nulls_local_rowids() {
        let source = seeded_source();
        let target = fresh_target();
        let counts = import_from_source(&source, &target, "global-repo").unwrap();

        assert_eq!(counts.memories, 3);
        assert_eq!(counts.bindings, 1);
        assert_eq!(counts.tags, 1);
        assert_eq!(counts.call_paths, 1);
        assert_eq!(counts.call_path_edges, 1);
        assert_eq!(counts.embedding_cache_rows, 2);
        assert_eq!(
            counts.meta_keys, 3,
            "the portable model-state keys carried (identity + remote config + provisional)",
        );

        // Every memory + binding is stamped the NEW repo id, never the legacy one.
        assert_eq!(
            count(&target, "SELECT COUNT(*) FROM repo_memories WHERE repo_id='global-repo'"),
            3
        );
        assert_eq!(
            count(&target, "SELECT COUNT(*) FROM repo_memories WHERE repo_id='legacy-repo'"),
            0
        );
        assert_eq!(
            count(&target, "SELECT COUNT(*) FROM repo_memory_bindings WHERE repo_id='global-repo'"),
            1,
        );

        // The binding's LOCAL rowid columns are all NULLed (re-resolved by the validate loop).
        let nulled = count(
            &target,
            "SELECT COUNT(*) FROM repo_memory_bindings WHERE memory_id='m1' AND logical_symbol_id \
             IS NULL AND symbol_id IS NULL AND chunk_id IS NULL AND edge_id IS NULL",
        );
        assert_eq!(nulled, 1, "local rowids nulled for re-resolution");
        // Its portable fields survive.
        let (path, commit, key): (Option<String>, Option<String>, Option<String>) = target
            .query_row(
                "SELECT path, commit_hash, item_key FROM repo_memory_bindings WHERE memory_id='m1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(path.as_deref(), Some("src/x.rs"), "portable path survives");
        assert_eq!(commit.as_deref(), Some("abc"));
        assert_eq!(key.as_deref(), Some("7"));

        // Call-path logical ids are NULLed too; the edge is copied verbatim.
        let (start, end): (Option<i64>, Option<i64>) = target
            .query_row(
                "SELECT start_logical_symbol_id, end_logical_symbol_id FROM \
                 repo_memory_call_paths WHERE memory_id='m1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((start, end), (None, None));

        // Both content-addressed cache rows landed, and the model meta key was carried.
        assert_eq!(count(&target, "SELECT COUNT(*) FROM embedding_cache"), 2);
        assert_eq!(
            count(
                &target,
                "SELECT COUNT(*) FROM repo_meta WHERE repo_id='global-repo' AND \
                 key='active_embedding_model'"
            ),
            1,
        );
    }

    /// The relocation-provenance columns (`symbol_kind`, `signature_hash`, `moniker_tool`,
    /// `moniker_tool_version`, `relocation_reason`) survive the import verbatim — dropping them
    /// would strip imported `scip_moniker` bindings of validation (`unverified` without
    /// `moniker_tool`) and of the oracle-backed relocation path (which requires both tool fields).
    #[test]
    fn import_preserves_moniker_binding_provenance() {
        let source = seeded_source();
        let target = fresh_target();
        import_from_source(&source, &target, "global-repo").unwrap();

        let provenance = |column: &str| -> Option<String> {
            target
                .query_row(
                    &format!("SELECT {column} FROM repo_memory_bindings WHERE memory_id='m1'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(provenance("symbol_kind").as_deref(), Some("function"));
        assert_eq!(provenance("signature_hash").as_deref(), Some("sighash"));
        assert_eq!(provenance("moniker_tool").as_deref(), Some("scip-rust"));
        assert_eq!(provenance("moniker_tool_version").as_deref(), Some("0.4"));
        assert_eq!(provenance("relocation_reason").as_deref(), Some("moved"));
    }

    /// Imported memories are reachable through KEYWORD search: `memory_search` retrieves
    /// exclusively through the `repo_memory_fts` mirror, whose only other writers are
    /// `upsert_memory_fts` (create/update) and the one-time V042 rebuild — so the import must
    /// re-derive it, or the whole imported corpus stays invisible to search forever.
    #[test]
    fn imported_memories_are_findable_via_memory_fts() {
        let source = seeded_source();
        let target = fresh_target();
        import_from_source(&source, &target, "global-repo").unwrap();

        let hits: i64 = target
            .query_row(
                "SELECT COUNT(*) FROM repo_memory_fts WHERE repo_memory_fts MATCH 'three' AND \
                 repo_id = 'global-repo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1, "an imported memory matches by its title words");
        // The tag derivation matches upsert_memory_fts (space-joined), so tag words match too.
        let tag_hits: i64 = target
            .query_row(
                "SELECT COUNT(*) FROM repo_memory_fts WHERE repo_memory_fts MATCH 'tagalpha' AND \
                 repo_id = 'global-repo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tag_hits, 1, "an imported memory matches by its tag");
    }

    #[test]
    fn import_is_idempotent_and_reports_honest_counts() {
        let source = seeded_source();
        let target = fresh_target();
        let first = import_from_source(&source, &target, "global-repo").unwrap();
        assert_eq!(first.memories, 3);
        assert_eq!(first.edges, 3, "all three edges (node, github, cross-repo) were carried");
        // A second import (a retry after a rename that never happened) inserts no duplicates AND
        // reports ZERO copies — the summary must not claim work the `INSERT OR IGNORE`s skipped.
        let second = import_from_source(&source, &target, "global-repo").unwrap();
        assert_eq!(second.memories, 0, "re-run reports zero memories copied");
        assert_eq!(second.bindings, 0);
        assert_eq!(second.tags, 0);
        assert_eq!(second.call_paths, 0);
        assert_eq!(second.call_path_edges, 0);
        assert_eq!(second.edges, 0, "a no-edit re-import reports zero edges (honest count)");
        assert_eq!(second.embedding_cache_rows, 0);
        assert_eq!(second.meta_keys, 0);
        assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories"), 3);
        assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memory_bindings"), 1);
        assert_eq!(count(&target, "SELECT COUNT(*) FROM embedding_cache"), 2);
        // The FTS mirror re-derivation is convergent too — one row per memory, not accumulated.
        assert_eq!(
            count(&target, "SELECT COUNT(*) FROM repo_memory_fts WHERE repo_id='global-repo'"),
            3
        );
        // Zero-diff content: the no-edit retry converged — the target rows still carry the
        // source content verbatim (the gated upsert wrote NOTHING, it didn't rewrite in place).
        let (title, body, payload): (String, String, Option<String>) = target
            .query_row(
                "SELECT title, body, payload_json FROM repo_memories WHERE id='m1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((title.as_str(), body.as_str()), ("one", "body"));
        // #465: the payload was carried through the consolidation upsert (not turned into NULL).
        assert_eq!(payload.as_deref(), Some(r#"{"priority":1}"#), "m1 payload survives import");
        // #464: all three edges carried under the global repo — a node edge to a carried target
        // (current), a github `tracks` edge re-homed to the import repo (current), and a node edge
        // whose target was NOT carried (kept `unresolved`).
        let edge = |kind: &str, anchor: &str| -> (String, String) {
            target
                .query_row(
                    "SELECT anchor_status, target_repo_id FROM repo_node_edges WHERE repo_id = \
                     'global-repo' AND target_kind = ?1 AND target_anchor = ?2",
                    [kind, anchor],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        };
        assert_eq!(edge("node", "m2").0, "current", "node edge to a carried target");
        assert_eq!(edge("github", "o/r#7"), ("current".to_string(), "global-repo".to_string()));
        assert_eq!(
            edge("node", "external-node").0,
            "unresolved",
            "target not carried stays unresolved"
        );
    }

    /// The CRASH-CREATED divergence window (Codex batch 8, finding 4): the import txn commits,
    /// the RENAME fails, the legacy file stays the LIVE store (keyless resolution keeps serving
    /// it), and the user edits a memory there. The retry must carry those edits into the global
    /// store — content upserted, children REPLACED (a tag removed legacy-side must not survive by
    /// union) — because the legacy always wins until the rename lands. Counts stay honest (only
    /// the actually-refreshed memory reports), and a further no-edit retry is a true no-op.
    #[test]
    fn a_retry_after_a_failed_rename_carries_legacy_edits_made_in_the_window() {
        let source = seeded_source();
        let target = fresh_target();
        import_from_source(&source, &target, "global-repo").unwrap();
        // ... the rename fails here; the legacy DB stays live and the user edits m1: new body,
        // tag set REPLACED (alpha removed, window added), binding re-anchored. Every authored
        // mutation path bumps `updated_at_ms` (update_memory / rebind_memory), mirrored here.
        source
            .execute(
                "UPDATE repo_memories SET body='edited in the window', updated_at_ms=99 WHERE \
                 id='m1'",
                [],
            )
            .unwrap();
        source.execute("DELETE FROM repo_memory_tags WHERE memory_id='m1'", []).unwrap();
        source
            .execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES ('m1','window-tag')", [])
            .unwrap();
        source
            .execute("UPDATE repo_memory_bindings SET path='src/y.rs' WHERE memory_id='m1'", [])
            .unwrap();

        let counts = import_from_source(&source, &target, "global-repo").unwrap();
        assert_eq!(counts.memories, 1, "only the edited memory reports as written");
        assert_eq!(counts.tags, 1, "the replaced tag set reports its reinserted row");
        assert_eq!(counts.bindings, 1);

        let body: String = target
            .query_row("SELECT body FROM repo_memories WHERE id='m1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(body, "edited in the window", "the legacy edit wins in the global store");
        let tags: Vec<String> = target
            .prepare("SELECT tag FROM repo_memory_tags WHERE memory_id='m1' ORDER BY tag")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(tags, ["window-tag"], "children are REPLACED — the removed tag is gone");
        let path: String = target
            .query_row("SELECT path FROM repo_memory_bindings WHERE memory_id='m1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(path, "src/y.rs");
        // The FTS mirror carries the refreshed body — the edit is immediately searchable.
        assert_eq!(
            count(
                &target,
                "SELECT COUNT(*) FROM repo_memory_fts WHERE repo_id='global-repo' AND \
                 repo_memory_fts MATCH 'window'"
            ),
            1
        );
        // Untouched memories reported nothing and kept their rows: convergence.
        let third = import_from_source(&source, &target, "global-repo").unwrap();
        assert_eq!(third.memories, 0, "a further no-edit retry is a true no-op");
        assert_eq!(third.tags, 0);
        assert_eq!(third.bindings, 0);
        assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memories"), 3);
        assert_eq!(count(&target, "SELECT COUNT(*) FROM repo_memory_tags"), 1);
    }

    /// The mirror invariant covers REMAPPED parents too (Codex batch 9): a memory whose id
    /// collided with another repo's row imports under the remapped id — a window edit to it
    /// legacy-side must still refresh the remapped row AND replace its children on retry. The
    /// batch-8 parent-edit gate missed exactly this (it tested the SOURCE id's owner, which stays
    /// foreign); the unconditional child replace closes the shape. The foreign row stays
    /// untouched throughout.
    #[test]
    fn a_retry_carries_window_edits_to_a_remapped_memory() {
        let source = seeded_source();
        let target = fresh_target();
        // The global store already owns "m1" under a different repo — the import remaps.
        target
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('other-repo', \
                 'o', 0)",
                [],
            )
            .unwrap();
        target
            .execute(
                "INSERT INTO repo_memories(id, kind, title, body, confidence, status, \
                 created_at_ms, updated_at_ms, source, memory_version, repo_id)
                 VALUES ('m1', 'Risk', 'other title', 'other body', 'low', 'active', 0, 0, \
                 'agent', 'v1', 'other-repo')",
                [],
            )
            .unwrap();
        import_from_source(&source, &target, "global-repo").unwrap();
        let remapped = remapped_memory_id("global-repo", "m1");

        // ... rename fails; the user edits m1 in the still-live legacy DB: body + tag replaced.
        source
            .execute(
                "UPDATE repo_memories SET body='remapped window edit', updated_at_ms=99 WHERE \
                 id='m1'",
                [],
            )
            .unwrap();
        source.execute("DELETE FROM repo_memory_tags WHERE memory_id='m1'", []).unwrap();
        source
            .execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES ('m1','remap-tag')", [])
            .unwrap();

        let counts = import_from_source(&source, &target, "global-repo").unwrap();
        assert_eq!(counts.memories, 1, "the remapped parent's refresh reports honestly");

        let body: String = target
            .query_row("SELECT body FROM repo_memories WHERE id=?1", [&remapped], |r| r.get(0))
            .unwrap();
        assert_eq!(body, "remapped window edit", "the window edit reaches the REMAPPED row");
        let tags: Vec<String> = target
            .prepare("SELECT tag FROM repo_memory_tags WHERE memory_id=?1 ORDER BY tag")
            .unwrap()
            .query_map([&remapped], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(tags, ["remap-tag"], "the remapped parent's children are replaced, not unioned");
        // The FTS mirror covers the remapped refresh (whole-repo re-derive).
        assert_eq!(
            count(
                &target,
                "SELECT COUNT(*) FROM repo_memory_fts WHERE repo_id='global-repo' AND \
                 repo_memory_fts MATCH 'remapped'"
            ),
            1
        );
        // The foreign owner of the ORIGINAL id is untouched — content and children.
        let (other_title, other_repo): (String, String) = target
            .query_row("SELECT title, repo_id FROM repo_memories WHERE id='m1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((other_title.as_str(), other_repo.as_str()), ("other title", "other-repo"));

        // Convergence: a no-edit retry reports zeros.
        let third = import_from_source(&source, &target, "global-repo").unwrap();
        assert_eq!(third.memories, 0);
        assert_eq!(third.tags, 0);
    }

    /// The mirror invariant on the MODEL-STATE unit (Codex batch 9): a model switch made in the
    /// crash-retry window — new active model, bumped freshness version, the remote config KEY
    /// REMOVED (moving remote → local) — is carried whole by the retry: values upserted, the
    /// absent key deleted, the NEW model's readiness restored. A no-edit retry reports zero meta
    /// keys.
    #[test]
    fn a_retry_carries_a_window_model_switch() {
        let source = seeded_source();
        let target = fresh_target();
        import_from_source(&source, &target, "global-repo").unwrap();

        // ... rename fails; the user switches models in the still-live legacy DB.
        source
            .execute("UPDATE repo_meta SET value='model-b' WHERE key='active_embedding_model'", [])
            .unwrap();
        source
            .execute("DELETE FROM repo_meta WHERE key='active_embedding_remote_config'", [])
            .unwrap();
        source
            .execute(
                "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, \
                 disabled, status, installed_at_ms, last_error)
                 VALUES ('model-b', 'embedding', 512, 'fastembed', 1, 0, 'Ready', 7, NULL)",
                [],
            )
            .unwrap();

        let counts = import_from_source(&source, &target, "global-repo").unwrap();
        assert!(counts.meta_keys >= 2, "the upserted model + deleted remote config both report");

        let meta = |key: &str| -> Option<String> {
            target
                .query_row(
                    "SELECT value FROM repo_meta WHERE repo_id='global-repo' AND key=?1",
                    [key],
                    |r| r.get(0),
                )
                .optional()
                .unwrap()
        };
        assert_eq!(meta("active_embedding_model").as_deref(), Some("model-b"));
        assert_eq!(
            meta("active_embedding_remote_config"),
            None,
            "the key removed legacy-side is removed here too — absence has meaning"
        );
        // The NEW model's readiness restored on the retry (re-derived from the source each run).
        let status: String = target
            .query_row("SELECT status FROM ai_models WHERE model_id='model-b'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "Ready");

        let third = import_from_source(&source, &target, "global-repo").unwrap();
        assert_eq!(third.meta_keys, 0, "a no-edit retry writes no meta");
    }

    /// A memory id already owned by a DIFFERENT repo in the global store (a pre-repo-vintage id,
    /// or a copied index) is REMAPPED — never dropped, and its children never attach to the other
    /// repo's memory. The remap is deterministic, so a retry converges instead of duplicating.
    #[test]
    fn import_remaps_a_memory_id_owned_by_another_repo() {
        let source = seeded_source();
        let target = fresh_target();
        // The global store already holds a DIFFERENT repo's memory under the id "m1" (m1 is the
        // source memory that carries the binding/tag/call-path children), with its own tag.
        target
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('other-repo', \
                 'o', 0)",
                [],
            )
            .unwrap();
        target
            .execute(
                "INSERT INTO repo_memories(id, kind, title, body, confidence, status, \
                 created_at_ms, updated_at_ms, source, memory_version, repo_id)
                 VALUES ('m1', 'Risk', 'other title', 'other body', 'low', 'active', 0, 0, \
                 'agent', 'v1', 'other-repo')",
                [],
            )
            .unwrap();
        target
            .execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES ('m1', 'other-tag')", [])
            .unwrap();

        let counts = import_from_source(&source, &target, "global-repo").unwrap();
        assert_eq!(counts.memories, 3, "the colliding memory is remapped, not dropped");

        // The other repo's memory is untouched — content, ownership, and its own children.
        let (other_title, other_repo): (String, String) = target
            .query_row("SELECT title, repo_id FROM repo_memories WHERE id='m1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(other_title, "other title", "the other repo's memory content is untouched");
        assert_eq!(other_repo, "other-repo");
        let other_tags: i64 = target
            .query_row("SELECT COUNT(*) FROM repo_memory_tags WHERE memory_id='m1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(other_tags, 1, "no imported child attached to the other repo's memory id");

        // The imported memory landed under the DETERMINISTIC remapped id, with its children.
        let new_id = remapped_memory_id("global-repo", "m1");
        let (title, repo): (String, String) = target
            .query_row("SELECT title, repo_id FROM repo_memories WHERE id = ?1", [&new_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(title, "one", "the source memory's content imported under the remapped id");
        assert_eq!(repo, "global-repo");
        for (table, expected) in [
            ("repo_memory_bindings", 1i64),
            ("repo_memory_tags", 1),
            ("repo_memory_call_paths", 1),
            ("repo_memory_call_path_edges", 1),
        ] {
            let rows: i64 = target
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE memory_id = ?1"),
                    [&new_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(rows, expected, "{table}: children follow the remapped id");
        }

        // RETRY: the deterministic remap converges — zero new rows, no duplicate remapped copies.
        let retry = import_from_source(&source, &target, "global-repo").unwrap();
        assert_eq!(retry.memories, 0, "the retry re-derives the SAME remapped id and no-ops");
        let remapped_copies: i64 = target
            .query_row(
                "SELECT COUNT(*) FROM repo_memories WHERE repo_id='global-repo' AND title='one'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remapped_copies, 1, "exactly one remapped copy across retries");
    }

    /// A dangling child row in the SOURCE (its memory_id absent from the source's repo_memories,
    /// while a memory with that id exists in the target under ANOTHER repo) is skipped — the
    /// child-ownership invariant: children only ever insert under a parent id this import owns.
    #[test]
    fn import_skips_orphan_children_instead_of_attaching_them_across_repos() {
        let source = seeded_source();
        // A dangling tag in the source referencing a memory the source does NOT hold. The child
        // tables carry no FK in some legacy vintages; simulate by deleting the parent after
        // inserting the tag under FK-off.
        source.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        source
            .execute(
                "INSERT INTO repo_memory_tags(memory_id, tag) VALUES ('foreign-mem', 'stray')",
                [],
            )
            .unwrap();

        let target = fresh_target();
        // The target holds 'foreign-mem' under ANOTHER repo — the row the orphan child would have
        // contaminated.
        target
            .execute(
                "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES ('other-repo', \
                 'o', 0)",
                [],
            )
            .unwrap();
        target
            .execute(
                "INSERT INTO repo_memories(id, kind, title, body, confidence, status, \
                 created_at_ms, updated_at_ms, source, memory_version, repo_id)
                 VALUES ('foreign-mem', 'Risk', 't', 'b', 'low', 'active', 0, 0, 'agent', 'v1', \
                 'other-repo')",
                [],
            )
            .unwrap();

        import_from_source(&source, &target, "global-repo").unwrap();
        let stray: i64 = target
            .query_row(
                "SELECT COUNT(*) FROM repo_memory_tags WHERE memory_id='foreign-mem'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stray, 0, "an orphan source child never attaches to another repo's memory");
    }

    /// The mirror invariant on the model-state unit (Codex batch 9, SUPERSEDES the batch-4
    /// "config-seeded value wins" posture): the legacy DB is the LIVE store for this repo until
    /// the rename lands — nothing else can legitimately write this repo's meta mid-consolidate
    /// (the import holds the repo's write locks; the target migrate is schema-only), so a target
    /// value that differs is a STALE copy from a previous crashed run and the source must win.
    #[test]
    fn carried_meta_mirrors_the_source_over_a_stale_target_value() {
        let source = seeded_source();
        let target = fresh_target();
        // A previous crashed run copied an older model choice; the window changed it legacy-side.
        target
            .execute(
                "INSERT INTO repo_meta(repo_id, key, value) VALUES ('global-repo', \
                 'active_embedding_model', 'stale-model')",
                [],
            )
            .unwrap();
        import_from_source(&source, &target, "global-repo").unwrap();
        let value: String = target
            .query_row(
                "SELECT value FROM repo_meta WHERE repo_id='global-repo' AND \
                 key='active_embedding_model'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(value, "model-a", "the authoritative legacy value replaces the stale copy");
    }

    /// The `repo_meta` classification (see [`CARRIED_META_KEYS`]): repo-PORTABLE configuration is
    /// carried verbatim — including `active_embedding_remote_config`, which `active_embedder()`
    /// reconstructs the remote transport from (dropping it silently rerouted post-consolidation
    /// searches to the local backend until reinstall) — while DB-LOCAL state (freshness cursors,
    /// transient install markers) never crosses.
    #[test]
    fn import_carries_portable_meta_and_leaves_db_local_state() {
        let source = seeded_source();
        let target = fresh_target();
        import_from_source(&source, &target, "global-repo").unwrap();

        let meta = |key: &str| -> Option<String> {
            target
                .query_row(
                    "SELECT value FROM repo_meta WHERE repo_id='global-repo' AND key=?1",
                    [key],
                    |r| r.get(0),
                )
                .optional()
                .unwrap()
        };
        assert_eq!(meta("active_embedding_model").as_deref(), Some("model-a"));
        assert_eq!(
            meta("active_embedding_remote_config").as_deref(),
            Some("{\"endpoint\":\"http://ollama:11434\"}"),
            "the remote-endpoint config survives verbatim — active_embedder routes remote",
        );
        assert_eq!(meta("git_commit"), None, "freshness cursors never cross (DB-local)");
        // The provisional flag is SEMANTIC state: absent ⇒ non-provisional (config-immune explicit
        // choice), so an auto-picked "1" must cross or `seed_active_embedding_model` could no
        // longer override the model from config post-consolidation.
        assert_eq!(
            meta("active_embedding_model_provisional").as_deref(),
            Some("1"),
            "the provisional auto-pick provenance survives — config can still override",
        );
        // And the model-state unit includes the ai_models READINESS row: identity without it
        // points at MissingModel and active_embedder refuses despite the carried cache.
        let (installed, status): (i64, String) = target
            .query_row(
                "SELECT installed, status FROM ai_models WHERE model_id='model-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((installed, status.as_str()), (1, "Ready"), "readiness row carried");
    }

    /// The readiness carry's two guarded shapes: a target row stuck at `MissingModel` (the
    /// manifest's fresh-DB seed) is RESTORED to the legacy Ready state; a target row the machine
    /// explicitly DISABLED is never overridden (the opt-out is shared by every repo in the global
    /// DB).
    #[test]
    fn readiness_carry_restores_missing_model_but_respects_disabled() {
        let source = seeded_source();

        // Target seeded MissingModel (what ensure_model_manifest writes on a fresh DB) → restored.
        let target = fresh_target();
        target
            .execute(
                "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, \
                 disabled, status) VALUES ('model-a', 'embedding', 384, 'local', 0, 0, \
                 'MissingModel')",
                [],
            )
            .unwrap();
        import_from_source(&source, &target, "global-repo").unwrap();
        let (installed, status): (i64, String) = target
            .query_row(
                "SELECT installed, status FROM ai_models WHERE model_id='model-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (installed, status.as_str()),
            (1, "Ready"),
            "a MissingModel manifest seed is restored from the legacy Ready row",
        );

        // Target explicitly disabled → untouched.
        let target = fresh_target();
        target
            .execute(
                "INSERT INTO ai_models(model_id, capability, embedding_dim, runtime, installed, \
                 disabled, status) VALUES ('model-a', 'embedding', 384, 'local', 0, 1, \
                 'MissingModel')",
                [],
            )
            .unwrap();
        import_from_source(&source, &target, "global-repo").unwrap();
        let (installed, disabled, status): (i64, i64, String) = target
            .query_row(
                "SELECT installed, disabled, status FROM ai_models WHERE model_id='model-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (installed, disabled, status.as_str()),
            (0, 1, "MissingModel"),
            "an explicit machine-level disable is never overridden by a carry",
        );
    }

    #[test]
    fn imported_marker_appends_the_suffix() {
        assert_eq!(
            imported_marker(Path::new("/repo/.rag-rat/index.sqlite")),
            PathBuf::from("/repo/.rag-rat/index.sqlite.imported"),
        );
    }

    /// The WAL sidecars travel with the `.imported` archive: a bare main-file rename would orphan
    /// `-wal`/`-shm` as permanent litter, and an un-checkpointed `-wal` (a busy checkpoint under a
    /// concurrent lockless reader — a sanctioned state) holds frames that BELONG to the archive.
    /// Renaming them alongside is what keeps the archive whole regardless of checkpoint outcome —
    /// the pinned BUSY posture.
    #[test]
    fn wal_sidecars_travel_with_the_imported_archive() {
        let dir =
            std::env::temp_dir().join(format!("ragrat-sidecars-{}-{:p}", std::process::id(), &()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("index.sqlite");
        let imported = imported_marker(&source);
        // Simulate the post-rename state with LEFTOVER sidecars (incl. an un-checkpointed wal).
        fs::write(&source, b"db").unwrap();
        fs::write(path_with_suffix(&source, "-wal"), b"frames").unwrap();
        fs::write(path_with_suffix(&source, "-shm"), b"shm").unwrap();
        fs::rename(&source, &imported).unwrap();

        rename_wal_sidecars(&source, &imported);

        for suffix in ["-wal", "-shm"] {
            assert!(
                !path_with_suffix(&source, suffix).exists(),
                "no {suffix} litter remains at the legacy path"
            );
            assert!(
                path_with_suffix(&imported, suffix).exists(),
                "the {suffix} sidecar travelled with the archive"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The pinned-`database` refusal is PATH-AWARE: a pin at the default legacy path needs only
    /// the key removed, while a CUSTOM pin must move its file first — keyless resolution never
    /// consults a custom path, so "remove the key" alone would strand the index unimported. The
    /// custom shape prints the literal commands for the user's paths.
    #[test]
    fn pinned_refusal_message_states_the_move_for_custom_paths() {
        let default_legacy = Path::new("/repo/.rag-rat/index.sqlite");

        let at_default = pinned_refusal_message(default_legacy, default_legacy);
        assert!(at_default.contains("Remove the `database` key"), "default shape: {at_default}");
        assert!(!at_default.contains("mv "), "default shape needs no move: {at_default}");

        let custom = pinned_refusal_message(Path::new("/repo/custom/my.db"), default_legacy);
        assert!(
            custom.contains("mv /repo/custom/my.db /repo/.rag-rat/index.sqlite"),
            "custom shape prints the literal move: {custom}"
        );
        assert!(
            custom.contains("mkdir -p /repo/.rag-rat"),
            "custom shape creates the default dir: {custom}"
        );
        assert!(
            custom.contains("-wal"),
            "custom shape warns about WAL sidecars holding recent writes: {custom}"
        );
    }

    // --- consolidation re-authors imported rows into the target owner stream (#541 Task 5) ---

    /// A target owner stream is rooted via a REAL `create_memory` call (not raw SQL) so the chain
    /// is genuinely non-empty — `fresh_target()` registers exactly one repo (`global-repo`), so
    /// `memory_repo_scope`'s sole-repo fallback resolves it without any extra connection-context
    /// setup.
    fn seeded_target_with_rooted_chain() -> Connection {
        let target = fresh_target();
        crate::query::memory::create_memory(&target, crate::query::memory::RepoMemoryCreate {
            kind: "Concept".to_string(),
            title: "seed".to_string(),
            body: "body".to_string(),
            confidence: "high".to_string(),
            created_by: None,
            source: None,
            tags: Vec::new(),
            payload_json: None,
            bind: crate::query::memory::RepoMemoryBindTarget::default(),
        })
        .unwrap();
        target
    }

    /// Before #541 Task 5, consolidation's import never touched the op-log: the imported rows
    /// landed in `repo_memories`/`repo_node_edges` but the per-CHAIN backfill gate saw the target's
    /// owner stream was already non-empty (this test's seed rooted it) and skipped authoring them
    /// entirely — a later `mark_obsolete` on such a row would author an inert `NodeStatus` with no
    /// `NodeCreate` behind it. This test proves the reconcile call `run` now makes closes that gap:
    /// an imported memory is present in the target's projection, and a follow-up `mark_obsolete` is
    /// NOT inert.
    #[test]
    fn consolidation_authors_imported_memories_into_target_owner_stream() {
        let source = seeded_source();
        let target = seeded_target_with_rooted_chain();

        // PREMISE SELF-CHECK: the seed must have rooted a non-empty local chain for `global-repo`,
        // or this test silently degrades to the (already-covered) genesis path and stops exercising
        // the per-chain-gate bypass #541 fixes.
        let device = crate::oplog::local_device(&target, 0).unwrap();
        let stream = crate::oplog::owner_stream("global-repo").unwrap();
        assert!(
            crate::oplog::chain_tail(&target, stream, device.fingerprint()).unwrap().is_some(),
            "the seed must root a non-empty owner chain, or this test degrades to the genesis path"
        );

        // The import itself does NOT touch the op-log (it predates #541's reconcile call) — only
        // `repo_memories`/`repo_node_edges` gain the imported rows.
        let counts = import_from_source(&source, &target, "global-repo").unwrap();
        assert_eq!(counts.memories, 3, "sanity: the import still carries all 3 source memories");
        assert!(
            !crate::oplog::load_projection(&target, stream)
                .unwrap()
                .nodes
                .contains_key(&crate::oplog::NodeId::from("m1")),
            "pre-reconcile: the imported memory m1 must NOT yet be projected (the per-chain gate \
             skipped it, the bug #541 fixes) — only the seed memory should be projected here"
        );

        // The call this task wires into `run`, immediately after `import_from_source` and before
        // the legacy-file rename.
        crate::query::memory::reconcile_owner_stream_for_repo(
            &target,
            "global-repo",
            schema::now_ms(),
        )
        .unwrap();

        // An imported memory is now present in the target owner stream's projection.
        let projected = crate::oplog::load_projection(&target, stream).unwrap();
        assert!(
            projected.nodes.contains_key(&crate::oplog::NodeId::from("m1")),
            "the imported memory m1 must be present in the target owner stream's projection"
        );

        // A follow-up `mark_obsolete` on the imported memory is NOT inert: it flips the projected
        // status (which requires the `NodeCreate` the reconcile just authored).
        crate::query::memory::mark_obsolete(&target, "m1").unwrap();
        let projected = crate::oplog::load_projection(&target, stream).unwrap();
        let node = projected
            .nodes
            .get(&crate::oplog::NodeId::from("m1"))
            .expect("m1 must still be projected after mark_obsolete");
        assert_eq!(
            node.status,
            crate::oplog::NodeStatus::Obsolete,
            "mark_obsolete on an imported memory must not be inert"
        );
    }
}
