use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use anyhow::Context;
use rag_rat_base::config::{self, Config};
use rag_rat_base::repo_identity::{self, RepoIdentity};
use rag_rat_base::{data_dir, locks};
use rag_rat_db::storage::IndexConnection;
use rusqlite::Connection;

use super::import::{self, ImportMode};
use super::{ConsolidateOutcome, ImportSummary, meta_merge};
use crate::index::{IndexDatabase, schema};

/// How long consolidate waits for the repo's per-repo write locks (global-side and legacy-side)
/// before refusing — an in-flight index/maintenance pass finishes well within it, and an explicit
/// retryable refusal beats importing under a live writer.
const CONSOLIDATE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
    let source = match resolve_consolidation_source(config, &target)? {
        ControlFlow::Continue(source) => source,
        ControlFlow::Break(outcome) => return Ok(outcome),
    };
    let imported = rag_rat_base::data_dir::imported_marker_path(&source);

    // Resolve the repo identity FIRST: the per-repo write locks are keyed by the id this run
    // registers and writes under (the A6 lock-matches-written-id rule).
    let identity = resolve_identity_for_config(config)?;

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating the global data directory {}", parent.display()))?;
    }

    let (_target_lock, _source_locks) =
        acquire_all_consolidate_locks(&target, &source, &identity.repo_id)?;

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
    let counts = import::import_from_source(
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

    finish_consolidation(target_conn, &source, &imported, &repo_id)?;

    Ok(ConsolidateOutcome::Imported(ImportSummary {
        repo_id,
        source,
        renamed_to: imported,
        target,
        counts,
    }))
}

/// Where this run imports FROM, or the outcome that ends it before any side effect: already on the
/// global store, a refused `[index] database` pin, no legacy file, or one already imported.
fn resolve_consolidation_source(
    config: &Config,
    target: &Path,
) -> anyhow::Result<ControlFlow<ConsolidateOutcome, PathBuf>> {
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
        if legacy != target
            && legacy.exists()
            && !rag_rat_base::data_dir::imported_marker_path(&legacy).exists()
        {
            source = legacy;
            pinned_at_target = true;
        } else {
            return Ok(ControlFlow::Break(ConsolidateOutcome::AlreadyGlobal {
                database: target.to_path_buf(),
            }));
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

    let imported = rag_rat_base::data_dir::imported_marker_path(&source);
    if !source.exists() {
        return Ok(ControlFlow::Break(if imported.exists() {
            ConsolidateOutcome::AlreadyImported { imported }
        } else {
            ConsolidateOutcome::NoLegacyIndex { source }
        }));
    }
    Ok(ControlFlow::Continue(source))
}

/// Hold the repo's per-repo write locks for the WHOLE registration → import → rename sequence:
/// the GLOBAL-side lock covers every row written under `identity.repo_id` in the global DB, and
/// the LEGACY-side locks exclude a watcher / MCP writer still keyed beside the legacy file, so
/// nothing can append to it between the snapshot read and the rename. Both bounded; the global
/// locks taken inside (schema in the migration, registry in `register_repo`) follow the
/// per-repo → global ordering rule (see `locks::GlobalLock`).
///
/// The LEGACY side drains EVERY id the source DB itself records, not just the CURRENT identity:
/// the legacy file predates the identity transition, so a writer started PRE-deepen still keys
/// its flock by the OLD `local:` id — a current-identity lock alone would not conflict with it,
/// and the snapshot + rename could race its writes into the renamed artifact (the same loss the
/// same-id lock exists to prevent; the outgoing-drain rule the upgrade path follows). The ids
/// come from a read-only PRE-lock peek at the source's own `repos` registry; canonical order,
/// bounded. A source so old it predates the registry tables peeks empty — only pre-A6 binaries
/// (whose lock files predate per-repo keying entirely) ever wrote such a file, so no
/// current-scheme lock could coordinate with them regardless.
fn acquire_all_consolidate_locks(
    target: &Path,
    source: &Path,
    repo_id: &str,
) -> anyhow::Result<(locks::WriteLock, Vec<locks::WriteLock>)> {
    let target_lock = acquire_consolidate_lock(target, repo_id, "global")?;
    let mut source_side_ids = source_registered_repo_ids(source);
    if !source_side_ids.iter().any(|id| id == repo_id) {
        source_side_ids.push(repo_id.to_string());
    }
    source_side_ids.sort_by(|a, b| {
        if rag_rat_base::locks::canonical_lock_order(a, b).0 == a.as_str() {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    });
    let mut source_locks = Vec::with_capacity(source_side_ids.len());
    for id in &source_side_ids {
        source_locks.push(acquire_consolidate_lock(source, id, "legacy")?);
    }
    Ok((target_lock, source_locks))
}

/// Rename the legacy file so a keyless config now resolves to the global store (via the
/// `.imported` latch), and a re-run is a no-op. AFTER the import commits, so a failure leaves
/// the legacy file in place to retry. The WAL sidecars travel WITH the archive: a bare
/// main-file rename would orphan `-wal`/`-shm` as permanent litter, and any un-checkpointed
/// frames in the WAL belong to the archive (SQLite opens the renamed pair as a unit) — the
/// same discipline the custom-pin remedy tells users to follow.
fn finish_consolidation(
    target_conn: &Connection,
    source: &Path,
    imported: &Path,
    repo_id: &str,
) -> anyhow::Result<()> {
    fs::rename(source, imported)
        .with_context(|| format!("renaming {} to {}", source.display(), imported.display()))?;
    rename_wal_sidecars(source, imported);
    // The legacy index is archived, so no retry of THIS consolidation can follow: the pin it wrote
    // stops being a replaceable copy and becomes the global store's own trust decision. A failure
    // here is left as a warning — the source identity in the marker already keeps any other source
    // from claiming it, and failing an import that has completed would help nothing.
    if let Err(error) = meta_merge::retire_pin_import(target_conn, repo_id) {
        tracing::warn!(
            repo_id = %repo_id,
            %error,
            "consolidation completed but could not retire its stream pin import marker"
        );
    }
    Ok(())
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

#[cfg(test)]
mod tests;
