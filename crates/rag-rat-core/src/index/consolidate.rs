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
use rag_rat_base::config::{self, Config};
use rag_rat_base::repo_identity::{self, RepoIdentity};
use rag_rat_base::{data_dir, locks};
use rag_rat_db::storage::IndexConnection;
use rusqlite::{Connection, OptionalExtension, params};

use crate::index::{self, IndexDatabase, schema};

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
///      * `memory_stream_seal_policy` — the one-way privacy intent for the repo's owner stream.
///        Unlike model-state keys it is MERGED monotonically: `sealed` on either side wins, absence
///        never clears it, and unknown values abort consolidation before reconciliation.
///      * `memory_stream_pin` — the owner this repo has trusted (see `sync subscribe`). MERGED, not
///        copied verbatim: it is the one subscription field built to outlive the subscription, so
///        dropping it on the move would let a changed `.rag-rat-stream` read as first use. The
///        source's EFFECTIVE pin crosses — its recorded pin, else the owner it is subscribed to,
///        which a store from before the pin existed records its trust decision as (that
///        subscription itself never crosses — see (b)). Until the rename lands the legacy index is
///        the live store, so a retry of the SAME legacy source replaces the pin its earlier,
///        unfinished run wrote (recorded, with that source, in `memory_stream_pin_imported`, and
///        retired once the rename lands); any other conflicting pin REFUSES the run, since carrying
///        either would silently override the other trust root.
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
///        `embedding_throughput_tune_v1` (a tuning cache, re-derived);
///      * `memory_contribution_owner` / `memory_subscription_owner` — the accounts whose stream
///        materializes the repo. Not copied and never needed:
///        `ensure_not_mirroring_another_account` REFUSES the whole run when the TARGET carries
///        either key, and a legacy source that carries one has nowhere to put it (the target's own
///        key must stay authoritative — it is what the target's drain already acted on). A repo
///        consolidates only while it owns its own stream.
///      * `memory_subscription_peers` / `memory_subscription_relay` — how to reach the subscribed
///        owner's host. They describe a subscription, and the subscription never crosses; carried
///        alone they would route toward an owner this repo no longer mirrors.
///      * `memory_stream_pin_imported` — the pin an UNFINISHED consolidation wrote and the legacy
///        source it came from, which is how a retry of that source tells its own stale copy from a
///        trust decision; it is retired once the rename lands, and any `sync subscribe` clears it.
const CARRIED_META_KEYS: &[&str] = &[
    "active_embedding_model",
    "embedding_active_model_version",
    "active_embedding_remote_config",
    "active_embedding_model_provisional",
    "memory_stream_seal_policy",
    "memory_stream_access_mode",
];

const MEMORY_STREAM_SEAL_POLICY_META_KEY: &str = "memory_stream_seal_policy";
const MEMORY_STREAM_ACCESS_MODE_META_KEY: &str = "memory_stream_access_mode";
const MEMORY_STREAM_PIN_META_KEY: &str = "memory_stream_pin";
const MEMORY_STREAM_PIN_IMPORTED_META_KEY: &str = "memory_stream_pin_imported";
const MEMORY_SUBSCRIPTION_OWNER_META_KEY: &str = "memory_subscription_owner";

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
    run_inner(config, None)
}

/// CLI consolidation with the EXACT config path that produced `config`. Unlike [`run`] (the
/// programmatic/test seam, whose callers may construct a Config without a file), this rechecks the
/// file after acquiring the source-side repo locks so a concurrent `rag-rat rm` that deleted it
/// wins cleanly rather than letting consolidation continue from stale in-memory configuration.
pub fn run_with_config_path(
    config: &Config,
    config_path: &Path,
) -> anyhow::Result<ConsolidateOutcome> {
    run_inner(config, Some(config_path))
}

fn run_inner(config: &Config, config_path: Option<&Path>) -> anyhow::Result<ConsolidateOutcome> {
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
        if rag_rat_base::locks::canonical_lock_order(a, b).0 == a.as_str() {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    });
    let mut _source_locks = Vec::with_capacity(source_side_ids.len());
    for id in &source_side_ids {
        _source_locks.push(acquire_consolidate_lock(&source, id, "legacy")?);
    }

    // #767 review: `rm` holds this same source-side repo lock while deleting the governing config.
    // If consolidate loaded Config first and then waited here, it would otherwise resume from that
    // stale clone, clear/register in the global DB, and import the now-empty legacy source AFTER rm
    // reported success. Recheck the exact path the CLI loaded only after all source locks are held;
    // if rm won, abort before migrating/clearing/registering anything. The lock keeps it present
    // for the rest of this run once observed.
    if let Some(config_path) = config_path
        && !config_path.is_file()
    {
        anyhow::bail!(
            "the governing config ({}) was removed while consolidate waited for the repo write \
             lock — a concurrent `rag-rat rm` won; aborting without re-registering the repo",
            config_path.display()
        );
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

    // #767: consolidate is a DELIBERATE re-add, like `init` — lift any `rag-rat rm` removal
    // tombstone for this repo first, or `register_repo` below would refuse the import with a "run
    // init" remedy that does not fit consolidate. Under the target lock already held.
    schema::clear_repo_removed(target_conn, &identity.repo_id)?;

    // Register (or adopt/extend) this repo in the global DB. The returned id is what every imported
    // row is stamped with — the legacy DB's own `repo_id` (placeholder or otherwise) is discarded.
    // Consolidation IMPORTS an indexed repo, so `register_repo` records the working-tree root
    // (#427).
    let repo_id = schema::register_repo(
        target_conn,
        &identity,
        &config.root,
        rag_rat_base::time::now_ms(),
        &crate::index::migration_hooks(),
    )?;

    // Refuse a target whose memories materialize from ANOTHER account's stream, BEFORE any import
    // side effect. The reconcile that signs the imported rows onto an owned stream cannot run for a
    // contributor (it owns none), and it is reached only AFTER `import_from_source` has committed —
    // so failing there would leave exactly the half-applied state it exists to prevent: persisted
    // rows and FTS children with no `NodeCreate`, re-imported and re-failed on every retry. A
    // SUBSCRIBER owns its stream but the drain does not honor it, so what the reconcile signs there
    // never becomes the repo's memory state on any other device. Stopping here leaves the legacy
    // file unrenamed and the target untouched, so the run is retryable once the repo is no longer
    // configured to mirror another account.
    crate::memory_write::ensure_not_mirroring_another_account(
        target_conn,
        &repo_id,
        "consolidating a legacy index",
    )?;

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
    let counts = import_from_source(
        source_storage.connection(),
        target_conn,
        &repo_id,
        ImportMode::ConsolidateLegacy,
    )?;
    drop(source_storage);

    // The schema-only open above deliberately bypasses the normal open/migrate hook. Bring the
    // store-global `/3` projection current before reconcile's anti-join trusts it; otherwise a
    // missing/older projector stamp can make already-authored rows look absent and append duplicate
    // immutable content ops. This uses the existing connection and the rebuild's own IMMEDIATE txn.
    rag_rat_oplog::rebuild_all_content_projections_if_stale(target_conn)?;

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
    crate::memory_write::reconcile_owner_stream_for_repo(
        target_conn,
        &repo_id,
        rag_rat_base::time::now_ms(),
    )?;

    // The REVERSE direction (#691 A1): mirror any accepted SYNCED /3 content on this repo's owner
    // stream back into the local memory tables as `origin='synced'` rows. Paired with the reconcile
    // above so the consolidated store's synced rows are materialized, not just its local ones. A
    // legacy import usually carries no synced content, so this is a near-no-op there.
    crate::memory_write::drain_synced_stream_for_repo(
        target_conn,
        &repo_id,
        rag_rat_base::time::now_ms(),
    )?;

    // Rename the legacy file so a keyless config now resolves to the global store (via the
    // `.imported` latch), and a re-run is a no-op. AFTER the import commits, so a failure leaves
    // the legacy file in place to retry. The WAL sidecars travel WITH the archive: a bare
    // main-file rename would orphan `-wal`/`-shm` as permanent litter, and any un-checkpointed
    // frames in the WAL belong to the archive (SQLite opens the renamed pair as a unit) — the
    // same discipline the custom-pin remedy tells users to follow.
    fs::rename(&source, &imported)
        .with_context(|| format!("renaming {} to {}", source.display(), imported.display()))?;
    rename_wal_sidecars(&source, &imported);
    // The legacy index is archived, so no retry of THIS consolidation can follow: the pin it wrote
    // stops being a replaceable copy and becomes the global store's own trust decision. A failure
    // here is left as a warning — the source identity in the marker already keeps any other source
    // from claiming it, and failing an import that has completed would help nothing.
    if let Err(error) = retire_pin_import(target_conn, &repo_id) {
        tracing::warn!(
            repo_id = %repo_id,
            %error,
            "consolidation completed but could not retire its stream pin import marker"
        );
    }

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

/// Which caller is driving [`import_from_source`], and thus what the source is and what to carry.
/// The two callers make opposite assumptions about the source that every SELECT depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportMode {
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
///  * `repo_meta` portable state         — model-state keys use per-key MIRROR
///    ([`copy_model_state`]); the seal policy uses a monotonic merge so privacy intent cannot be
///    downgraded by an absent or unsafe source value.
///  * model-state mirror details: upsert for keys present in the source, DELETE for carried keys
///    absent there (a model switch in the window may legitimately remove a key, e.g. the remote
///    config when moving to a local model — keeping it would tear the unit).
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
        bindings: copy_bindings(source, &tx, repo_id, &id_map)?,
        tags: copy_tags(source, &tx, &id_map)?,
        call_paths: copy_call_paths(source, &tx, &id_map)?,
        call_path_edges: copy_call_path_edges(source, &tx, &id_map)?,
        edges: copy_node_edges(source, &tx, repo_id, &id_map, mode, &own)?,
        // Seed carries NO machine state: the embedding cache is content-derived from other private
        // repos on this box, and the model-state meta names this machine's embedder — the public
        // node runs elsewhere and establishes its own (see `ImportMode::SeedPublic`).
        embedding_cache_rows: match mode {
            ImportMode::ConsolidateLegacy => copy_embedding_cache(source, &tx)?,
            ImportMode::SeedPublic => 0,
        },
        meta_keys: match mode {
            ImportMode::ConsolidateLegacy => copy_model_state(source, &tx, repo_id)?,
            ImportMode::SeedPublic => 0,
        },
    };
    rag_rat_query::memory::remap_call_path_callee_logical_symbol_ids(&tx, source, &callee_remap)?;
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
        for table in ["repo_memory_tags", "repo_memory_call_paths", "repo_memory_call_path_edges"] {
            tx.execute(&format!("DELETE FROM {table} WHERE memory_id = ?1"), [id])?;
        }
        tx.execute(
            "DELETE FROM repo_memory_bindings WHERE memory_id = ?1 AND repo_id = ?2",
            params![id, repo_id],
        )?;
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
    // This store's resolution of the anchor (#1297): the import takes the source's local
    // call-path tables wholesale, keyed by the hash the source resolved, so it takes the
    // resolution with them — all of it, since a resolved row's shadows are one view. A source
    // from before the columns contributes NULLs, i.e. no resolution beyond the authored one.
    let resolution: Vec<String> = [
        "resolved",
        "resolved_binding_id",
        "resolved_path",
        "resolved_start_line",
        "resolved_end_line",
        "resolved_symbol_kind",
        "resolved_signature_hash",
        "resolved_moniker_tool_version",
    ]
    .iter()
    .map(|column| source_column_or_null(source, column))
    .collect::<anyhow::Result<_>>()?;
    let resolution = resolution.join(", ");
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
         {moniker_tool}, {moniker_tool_version}, {relocation_reason}, {resolution}
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
             signature_hash, moniker_tool, moniker_tool_version, relocation_reason, repo_id, \
             resolved, resolved_binding_id, resolved_path, resolved_start_line, \
             resolved_end_line, resolved_symbol_kind, resolved_signature_hash, \
             resolved_moniker_tool_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, NULL, ?7, ?8, ?9, ?10, ?11, ?12, \
             ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
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
                row.get::<_, Option<i64>>(20)?,
                row.get::<_, Option<String>>(21)?,
                row.get::<_, Option<String>>(22)?,
                row.get::<_, Option<i64>>(23)?,
                row.get::<_, Option<i64>>(24)?,
                row.get::<_, Option<String>>(25)?,
                row.get::<_, Option<String>>(26)?,
                row.get::<_, Option<String>>(27)?,
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
    mode: ImportMode,
    own: &rag_rat_oplog::CreatedContent,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_node_edges")? {
        return Ok(0);
    }
    // Edges carry their OWN `origin`: another account's edge onto one of our local memories is
    // `synced` yet its source node is in `id_map`, so it is dropped here or it would be re-authored
    // as our own `EdgeAdd`. A synced edge the source's own account added is carried like a local
    // one (`own`, empty for seed). (Repo scope rides `id_map`, already filtered by
    // `copy_memories`.)
    let mut stmt = source.prepare(
        "SELECT source_node_id, relation, target_repo_id, target_kind, target_anchor, \
         created_at_ms, repo_id FROM repo_node_edges
         WHERE origin = 'local' OR edge_key IN (SELECT value FROM json_each(?1))",
    )?;
    let mut rows = stmt.query(params![serde_json::to_string(&own.edges)?])?;
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
        let src_repo = row.get::<_, String>(6)?;
        let (target_repo_id, target_anchor, target_node_id, anchor_status) =
            match target_kind.as_str() {
                "node" => match id_map.get(&src_target_anchor) {
                    Some(mapped) =>
                        (repo_id.to_string(), mapped.clone(), Some(mapped.clone()), "current"),
                    // A node target outside the imported set. `add_edge` allows explicit cross-repo
                    // node edges, so on a MULTI-repo seed source this points at a DIFFERENT private
                    // repo (id_map holds only the published repo) — carrying its repo_id + node id
                    // would leak that repo onto the public op-log once the edge is authored. Drop
                    // it. Legacy consolidation KEEPS it: either an explicit cross-repo reference,
                    // or a same-repo memory the import left out because another
                    // account created it, which may come back through sync — so
                    // a same-repo target is re-homed under the new identity,
                    // where `resolve_node_target` finds the row if it ever arrives.
                    None if matches!(mode, ImportMode::SeedPublic) => continue,
                    None if src_target_repo == src_repo =>
                        (repo_id.to_string(), src_target_anchor.clone(), None, "unresolved"),
                    None => (src_target_repo, src_target_anchor.clone(), None, "unresolved"),
                },
                _ => (repo_id.to_string(), src_target_anchor.clone(), None, "current"),
            };
        let key = rag_rat_query::memory::edge_key(
            source_node_id,
            &relation,
            &target_kind,
            &target_anchor,
        );
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
/// by the validate loop, like the bindings' rowid columns). The path identity is copied first and
/// then re-keyed by [`rag_rat_query::memory::remap_call_path_callee_logical_symbol_ids`] when a
/// callee id changes.
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

/// Copy `repo_memory_call_path_edges`. Pre-V099 sources have no callee identity columns; copy them
/// as unknown so validation fails closed until an exact compatibility match converges the row.
/// Current rows are copied first, then the import transaction re-derives their callee ids,
/// fingerprints, sequence hashes, and binding ids under the destination repo identity.
fn copy_call_path_edges(
    source: &Connection,
    tx: &Connection,
    id_map: &BTreeMap<String, String>,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_memory_call_path_edges")? {
        return Ok(0);
    }
    let callee_columns =
        if schema::column_exists(source, "repo_memory_call_path_edges", "callee_identity_known")? {
            "callee_logical_symbol_id, callee_identity_known"
        } else {
            "NULL, 0"
        };
    let mut stmt = source.prepare(&format!(
        "SELECT memory_id, edge_sequence_hash, ordinal, edge_fingerprint, from_name, to_name, \
         edge_kind, target_qualified_name, receiver_hint, {callee_columns} FROM \
         repo_memory_call_path_edges"
    ))?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let Some(memory_id) = id_map.get(&row.get::<_, String>(0)?) else {
            continue;
        };
        let changed = tx.execute(
            "INSERT OR IGNORE INTO repo_memory_call_path_edges(memory_id, edge_sequence_hash, \
             ordinal, edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name, \
             receiver_hint, callee_logical_symbol_id, callee_identity_known)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, i64>(10)?,
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
        if *key == MEMORY_STREAM_SEAL_POLICY_META_KEY || *key == MEMORY_STREAM_ACCESS_MODE_META_KEY
        {
            continue;
        }
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
    count += merge_stream_seal_policy(source, tx, repo_id)?;
    count += merge_stream_access_mode(source, tx, repo_id)?;
    count += merge_stream_pin(source, tx, repo_id)?;
    if let Some(model_id) = active_model {
        carry_active_model_readiness(source, tx, &model_id)?;
    }
    Ok(count)
}

/// `repo_meta[key]` in the legacy SOURCE index — a single-repo store, so the key alone selects it.
fn source_repo_meta(source: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(source
        .query_row("SELECT value FROM repo_meta WHERE key = ?1 LIMIT 1", [key], |row| {
            row.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

/// `repo_meta[key]` for `repo_id` in the consolidation target.
fn target_repo_meta(tx: &Connection, repo_id: &str, key: &str) -> anyhow::Result<Option<String>> {
    Ok(tx
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id = ?1 AND key = ?2",
            params![repo_id, key],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// A `repo_meta` setting whose only persisted token is `token` — absence means no explicit intent.
struct SingleTokenMeta {
    key: &'static str,
    token: &'static str,
    /// Names the setting in the unknown-token refusal.
    what: &'static str,
}

const SEAL_POLICY_META: SingleTokenMeta = SingleTokenMeta {
    key: MEMORY_STREAM_SEAL_POLICY_META_KEY,
    token: "sealed",
    what: "memory stream seal policy",
};

const ACCESS_MODE_META: SingleTokenMeta = SingleTokenMeta {
    key: MEMORY_STREAM_ACCESS_MODE_META_KEY,
    token: "public",
    what: "memory stream access mode",
};

impl SingleTokenMeta {
    /// The `(source, target)` values, refusing when either side holds a token this binary does not
    /// understand — before any of consolidation's reconciliation authoring.
    fn read_both(
        &self,
        source: &Connection,
        tx: &Connection,
        repo_id: &str,
    ) -> anyhow::Result<(Option<String>, Option<String>)> {
        let source_value = source_repo_meta(source, self.key)?;
        let target_value = target_repo_meta(tx, repo_id, self.key)?;
        for (side, value) in
            [("legacy source", source_value.as_deref()), ("target", target_value.as_deref())]
        {
            if let Some(value) = value
                && value != self.token
            {
                anyhow::bail!(
                    "{side} repo `{repo_id}` has unknown {} `{value}`; refusing to consolidate",
                    self.what
                );
            }
        }
        Ok((source_value, target_value))
    }

    /// Carry a present source value onto an ABSENT target — the merge's only write. Returns the
    /// rows written.
    fn carry_onto_absent_target(
        &self,
        tx: &Connection,
        repo_id: &str,
        (source_value, target_value): &(Option<String>, Option<String>),
    ) -> anyhow::Result<u64> {
        if source_value.is_some() && target_value.is_none() {
            Ok(tx.execute(
                "INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, ?3)",
                params![repo_id, self.key, self.token],
            )? as u64)
        } else {
            Ok(0)
        }
    }
}

/// Carry the source's trust pin onto the target — see the `memory_stream_pin` entry in the
/// classification on [`CARRIED_META_KEYS`]. Returns the rows written.
fn merge_stream_pin(source: &Connection, tx: &Connection, repo_id: &str) -> anyhow::Result<u64> {
    let target_meta = |key: &str| target_repo_meta(tx, repo_id, key);
    // The EFFECTIVE pin: a store from before the pin existed, or one still subscribed, records its
    // trust decision only as the subscription owner.
    let Some(source_pin) = source_repo_meta(source, MEMORY_STREAM_PIN_META_KEY)?
        .or(source_repo_meta(source, MEMORY_SUBSCRIPTION_OWNER_META_KEY)?)
    else {
        return Ok(0);
    };
    let target_pin = target_meta(MEMORY_STREAM_PIN_META_KEY)?;
    let replaceable = match &target_pin {
        None => true,
        Some(pin) if *pin == source_pin => return Ok(0),
        // The pin an earlier, unfinished run wrote FROM THIS SAME legacy source, untouched since —
        // a subscribe in the target clears the marker, and a completed consolidation retires it.
        // Until the rename lands the legacy index is the live store, so a repin made there in the
        // crash-retry window must replace the copy the last run left. Another source's pin is not
        // a stale copy of this one: two clones of a repository share its repo id, and letting the
        // second overwrite the first would silently move the trust root.
        Some(pin) =>
            target_meta(MEMORY_STREAM_PIN_IMPORTED_META_KEY)?.as_deref()
                == Some(pin_import_marker(source, pin).as_str()),
    };
    if !replaceable {
        anyhow::bail!(
            "consolidation refused: the legacy index for `{repo_id}` trusts stream owner \
             {source_pin}, but the global store already pins {} from a decision made there, not \
             from an earlier consolidation. Carrying either would silently override the other \
             trust root. Confirm which owner this repository should trust, record it in the \
             legacy index with `rag-rat sync subscribe <owner>`, and retry",
            target_pin.unwrap_or_default(),
        );
    }
    let marker = pin_import_marker(source, &source_pin);
    let mut written = 0;
    for (key, value) in [
        (MEMORY_STREAM_PIN_META_KEY, source_pin.as_str()),
        (MEMORY_STREAM_PIN_IMPORTED_META_KEY, marker.as_str()),
    ] {
        written += tx.execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(repo_id, key) DO UPDATE SET value = excluded.value",
            params![repo_id, key, value],
        )?;
    }
    Ok(written as u64)
}

/// What `memory_stream_pin_imported` holds: the pin an import wrote AND the legacy source it came
/// from, so only a retry of that same source can claim it. A legacy index is always a file, so its
/// path identifies it for as long as the consolidation stays unfinished.
fn pin_import_marker(source: &Connection, pin: &str) -> String {
    serde_json::json!({ "source": source.path().unwrap_or_default(), "pin": pin }).to_string()
}

/// Retire the import marker once a consolidation has completed — see `merge_stream_pin`.
fn retire_pin_import(conn: &Connection, repo_id: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM repo_meta WHERE repo_id = ?1 AND key = ?2", params![
        repo_id,
        MEMORY_STREAM_PIN_IMPORTED_META_KEY
    ])
}

/// Merge the owner-stream seal policy as a one-way ratchet. The only persisted value this binary
/// understands is `sealed`; absence means no explicit intent. A sealed source must seal the target,
/// while a target already sealed remains sealed across retries even if the legacy source is absent.
/// Unknown values on either side fail closed before consolidation's reconciliation authoring.
fn merge_stream_seal_policy(
    source: &Connection,
    tx: &Connection,
    repo_id: &str,
) -> anyhow::Result<u64> {
    let sides = SEAL_POLICY_META.read_both(source, tx, repo_id)?;
    SEAL_POLICY_META.carry_onto_absent_target(tx, repo_id, &sides)
}

/// Merge the owner-stream ACCESS MODE. Unlike the seal ratchet there is NO safe winner: access mode
/// folds into the stream identity, so a public and a non-public index own DIFFERENT `/2` streams —
/// silently picking one would either strand content or (private→public) leak private memories onto
/// a public-labeled stream. So two EXPLICIT modes that disagree REFUSE. The only persisted token is
/// `public` (absence = private default); the consolidation target is fresh, so a lone `public`
/// source carries onto the absent target (making the consolidated index public), exactly as
/// intended for a published node switching embedding models.
fn merge_stream_access_mode(
    source: &Connection,
    tx: &Connection,
    repo_id: &str,
) -> anyhow::Result<u64> {
    let sides = ACCESS_MODE_META.read_both(source, tx, repo_id)?;

    // Two explicit-but-disagreeing modes have no safe winner. With only `public`/absent this can
    // only be source-`public` vs target-`public` (agree) today; the guard future-proofs a
    // `private` token.
    if let (Some(s), Some(t)) = (sides.0.as_deref(), sides.1.as_deref())
        && s != t
    {
        anyhow::bail!(
            "legacy source and target repo `{repo_id}` disagree on memory stream access mode \
             (`{s}` vs `{t}`); refusing to consolidate a public and a non-public index"
        );
    }

    ACCESS_MODE_META.carry_onto_absent_target(tx, repo_id, &sides)
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
mod tests;
