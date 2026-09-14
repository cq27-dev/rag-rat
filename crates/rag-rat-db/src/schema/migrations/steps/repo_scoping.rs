use rusqlite::Connection;

use crate::schema::migrations::{add_column_if_missing, column_exists, sqlite_object_exists};

/// V038 (memory-sync phase A1): the per-machine `repos` registry + per-repo `repo_meta` k/v store —
/// the substrate the global-DB consolidation scopes every other table against. All greenfield,
/// STRICT per repo convention.
///
/// The seed placeholder row (`repo_id = '__unassigned__'`, which MUST equal
/// [`rag_rat_base::repo_identity::LEGACY_REPO_ID`]) marks a legacy single-repo DB awaiting
/// adoption: the first post-migration open calls [`super::register_repo`], which rewrites the
/// placeholder to the real content-derived `repo_id` in one step. A consolidated DB holding more
/// than one repo never carries the placeholder — `register_repo` refuses to adopt when a different
/// real id already owns the DB.
///
/// `repo_roots`/`repo_meta` carry an `ON DELETE CASCADE` FK to `repos` (NOT to a reindex-volatile
/// parent), so the volatile-FK trip-wire does not flag them and they need no allowlist entry.
///
/// Idempotent AND adoption-safe: `CREATE TABLE IF NOT EXISTS` + a placeholder seed guarded by "no
/// real repo row exists yet". `schema::apply` re-runs every additive migration, and
/// `IndexDatabase::rebuild` takes that path (via `create_or_migrate`) on an ALREADY-adopted DB — so
/// the seed must not re-mint the placeholder after `register_repo` UPDATE'd its PK to the real id.
/// A fresh DB (empty `repos`), a forward-migrated V037 index (empty `repos`), and a re-apply of an
/// adopted DB (a real row present) all converge correctly: the first two seed the placeholder, the
/// last leaves the real row untouched.
pub fn apply_repos_registry(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(REPOS_REGISTRY_DDL)
}

/// V038 DDL. The placeholder literal `'__unassigned__'` MUST equal
/// [`rag_rat_base::repo_identity::LEGACY_REPO_ID`] — `super::register_repo` reads that constant
/// when it adopts the row (a matching bootstrap test pins the two together).
pub(crate) const REPOS_REGISTRY_DDL: &str = "
    CREATE TABLE IF NOT EXISTS repos(
        repo_id          TEXT PRIMARY KEY,
        display_name     TEXT NOT NULL,
        registered_at_ms INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE IF NOT EXISTS repo_roots(
        repo_id          TEXT NOT NULL REFERENCES repos(repo_id) ON DELETE CASCADE,
        root             TEXT NOT NULL,
        registered_at_ms INTEGER NOT NULL,
        PRIMARY KEY(repo_id, root)
    ) STRICT;
    CREATE TABLE IF NOT EXISTS repo_meta(
        repo_id TEXT NOT NULL REFERENCES repos(repo_id) ON DELETE CASCADE,
        key     TEXT NOT NULL,
        value   TEXT,
        PRIMARY KEY(repo_id, key)
    ) STRICT;
    -- Adoption placeholder (MUST equal schema::LEGACY_REPO_ID). A legacy single-repo DB carries
    -- exactly this one row until register_repo() rewrites it to the real content-derived repo_id.
    -- Seed ONLY when no real (non-placeholder) repo already owns the DB: schema::apply re-runs \
                                             every
    -- additive migration, and IndexDatabase::rebuild reaches it (via create_or_migrate) on an
    -- ALREADY-adopted DB — where the placeholder's PK has been UPDATE'd to the real id, so a plain
    -- INSERT OR IGNORE would find no conflict and resurrect the marker beside the real row. The
    -- WHERE NOT EXISTS guard makes the seed a no-op once a real repo exists; INSERT OR IGNORE \
                                             keeps
    -- it idempotent when only the placeholder is present (fresh / forward-migrated re-apply).
    INSERT OR IGNORE INTO repos(repo_id, display_name, registered_at_ms)
        SELECT '__unassigned__', '', 0
        WHERE NOT EXISTS (SELECT 1 FROM repos WHERE repo_id != '__unassigned__');
";

/// The `index_meta` keys V039 relocates into `repo_meta` (memory-sync phase A2) — the per-repo
/// singletons that were global-by-accident under the one-DB-per-repo assumption. Kept as ONE list
/// so the copy and the matching delete cannot drift. Machine/db-level keys
/// (`generated_flags_version`, the reencode DONE gate, the active-model provenance flag + remote
/// config, the throughput-tune cache) deliberately STAY in `index_meta` — they are not per-repo, or
/// are scoped by a later workstream.
const V039_INDEX_META_KEYS: &[&str] = &[
    "source_root",
    "content_revision",
    "git_commit",
    "git_dirty",
    "graph_index_version",
    "active_embedding_model",
    "clone_graph_live_generation",
    "github_last_sync_ms",
    "git_history_indexed_head",
    "git_history_indexed_root",
    "git_history_indexed_shallow",
    "local_crate_roots",
    "indexed_at_ms",
    "fts_dirty",
    "fts_source_revision",
    "fts_synced_at_ms",
];

/// The `reconcile_meta` keys V039 relocates into `repo_meta`. The `reconcile_meta` TABLE is
/// intentionally NOT dropped here — a later cleanup migration owns that (the remaining reconcile
/// timing keys still live in it); only these two per-repo keys move.
const V039_RECONCILE_META_KEYS: &[&str] =
    &["embedding_active_model_version", "vector_int8_reencode_cursor"];

/// V039 (memory-sync phase A2): relocate the per-repo singleton meta keys out of the global
/// `index_meta` / `reconcile_meta` into `repo_meta`, under the SOLE `repos` row that owns this DB
/// (see [`sole_repo_id`]) — the real repo_id when the DB was already adopted, else the
/// [`rag_rat_base::repo_identity::LEGACY_REPO_ID`] placeholder V038 seeds. Targeting the sole row
/// (not a hardcoded placeholder) is what keeps the `repo_meta → repos` FK satisfied on an ADOPTED
/// DB, where the placeholder row is gone. The read/write call sites move to the `repo_meta`
/// accessors in the same change, so a moved key is never read from the table it was deleted from.
///
/// Idempotent (`INSERT OR IGNORE` deduped by the `(repo_id, key)` PK + `DELETE` of the source
/// rows): a fresh DB has empty meta tables → the copy/delete are no-ops, and a forward-migrated
/// legacy DB converges on the identical shape. Copy-before-delete on each table means even a torn
/// run (crash between the copy and the delete) re-converges: the re-run's copy is ignored and the
/// delete finishes, and readers already read `repo_meta` (the authoritative side).
pub fn apply_move_per_repo_meta(conn: &Connection) -> rusqlite::Result<()> {
    relocate_meta_keys(conn, "index_meta", V039_INDEX_META_KEYS)?;
    relocate_meta_keys(conn, "reconcile_meta", V039_RECONCILE_META_KEYS)?;
    Ok(())
}

/// Copy the listed keys from `source_table` (a `(key, value)` k/v table) into `repo_meta` under the
/// repo that owns this DB ([`sole_repo_id`]), then delete them from the source. Resolving the
/// target HERE — inside the shared helper — means every future key-move caller inherits the correct
/// target (real-after-adoption / placeholder-before), instead of each re-deriving a placeholder
/// that a prior adoption may have deleted. `source_table` and `keys` are internal string literals
/// (never user input), so interpolating them into the SQL is safe.
fn relocate_meta_keys(
    conn: &Connection,
    source_table: &str,
    keys: &[&str],
) -> rusqlite::Result<()> {
    // V040 RECLASSIFICATION: some keys V039 lists here are GLOBAL infrastructure, not per-repo, and
    // V040 moves them back to `index_meta` (see [`RECLASSIFIED_GLOBAL_INDEX_META_KEYS`] +
    // [`move_repo_meta_keys_to_global`]). V039 is frozen (merged), so it still names them — filter
    // them out HERE so this shared helper never re-relocates a now-global key OUT of `index_meta`.
    // Two failures this prevents on a ladder replay (`schema::apply` / `index --full`, which
    // re-runs V039): (1) it would undo the reclassification every full rebuild; (2) with the
    // key present in `index_meta` the movable-keys gate below would fire, resolve
    // `sole_repo_id`, and HARD-ERROR on a consolidated >1-repo DB — regressing the round-4
    // replay property. Every genuinely-per-repo key is unaffected, so V039's relocation is
    // byte-identical for them.
    let keys: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|key| !RECLASSIFIED_GLOBAL_INDEX_META_KEYS.contains(key))
        .collect();
    if keys.is_empty() {
        return Ok(());
    }
    let in_list = keys.iter().map(|key| format!("'{key}'")).collect::<Vec<_>>().join(", ");
    // IDEMPOTENCE GATE, resolved BEFORE `sole_repo_id`: on a re-apply the source keys are already
    // gone (schema::apply re-runs the WHOLE ladder — a `create_or_migrate` / `rag-rat index --full`
    // full rebuild), so there is nothing to move. Return without resolving the sole repo. This is
    // load-bearing on a CONSOLIDATED (>1 real repo) DB, where `sole_repo_id`'s exactly-one-row
    // expectation would otherwise HARD-ERROR the ladder replay even though the move is a proven
    // no-op — breaking `index --full` for every repo in the DB. Only a genuinely-unmigrated
    // (single-repo / legacy) DB, whose source rows still exist, reaches the resolution. A torn run
    // (crash between the copy and the delete) still re-converges: the source rows survived the
    // missing delete, so the gate finds them present and finishes the move.
    let has_movable_keys: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {source_table} WHERE key IN ({in_list}))"),
        [],
        |row| row.get(0),
    )?;
    if !has_movable_keys {
        return Ok(());
    }
    let target_repo_id = sole_repo_id(conn)?;
    conn.execute(
        &format!(
            "INSERT OR IGNORE INTO repo_meta(repo_id, key, value)
             SELECT ?1, key, value FROM {source_table} WHERE key IN ({in_list})"
        ),
        [target_repo_id.as_str()],
    )?;
    conn.execute(&format!("DELETE FROM {source_table} WHERE key IN ({in_list})"), [])?;
    Ok(())
}

/// The single `repos` row that owns this DB at migration time — the real repo_id if it was already
/// adopted via [`super::register_repo`], else the [`rag_rat_base::repo_identity::LEGACY_REPO_ID`]
/// placeholder V038 seeds.
///
/// The relocation MUST target this id, not a hardcoded placeholder: on an adopted DB the
/// placeholder row is deleted, so an `INSERT` into `repo_meta` under it trips the `repo_meta →
/// repos` FK — with `foreign_keys = ON` (production) that aborts the whole `migrate_forward`; with
/// it off it orphans rows the per-repo accessors ([`super::sole_repo_id`]) can never resolve.
/// V038 always leaves exactly one `repos` row and `register_repo` keeps it at one, so 0 or >1 is a
/// broken invariant — surfaced as an attributable migration error rather than a silent FK abort or
/// a wrong-scope pick. (Distinct from the runtime [`super::sole_repo_id`], which `ORDER BY`s the
/// placeholder last and `LIMIT 1`s without a hard error; a migration wants the hard error on `!= 1`
/// so a broken invariant is loud.)
fn sole_repo_id(conn: &Connection) -> rusqlite::Result<String> {
    let mut stmt = conn.prepare("SELECT repo_id FROM repos")?;
    let mut ids =
        stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    if ids.len() == 1 {
        return Ok(ids.pop().expect("length checked to be exactly 1"));
    }
    Err(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
        Some(format!(
            "per-repo meta relocation expects exactly one repos row (the sole repo owning this \
             DB), found {}",
            ids.len()
        )),
    ))
}

/// The `repo_id` column added to every direct-scoped core table in V040. `NOT NULL DEFAULT` the
/// [`rag_rat_base::repo_identity::LEGACY_REPO_ID`] placeholder: existing rows backfill to the
/// placeholder, which `register_repo` rewrites to the real id at adoption (the A1/A2 pattern), and
/// any writer that forgets to stamp still produces a scannable single-repo value rather than NULL.
const REPO_ID_COLUMN_DEF: &str = "TEXT NOT NULL DEFAULT '__unassigned__'";

/// The `index_meta` keys V040 relocates into `repo_meta` — the active-embedding-model provenance
/// pair that stayed behind in A2's V039 sweep (they postdated the plan's inventory). Reuniting them
/// with `active_embedding_model` / `embedding_active_model_version` (already in `repo_meta`) so the
/// whole model-provenance family is per-repo. Idempotent: on a re-apply the source keys are already
/// gone, so the copy inserts nothing.
const V040_INDEX_META_KEYS: &[&str] =
    &["active_embedding_model_provisional", "active_embedding_remote_config"];

/// The keys V039 relocated to `repo_meta` that V040 RECLASSIFIES as GLOBAL infrastructure and moves
/// BACK to `index_meta` (see [`move_repo_meta_keys_to_global`]). V039 (frozen) misfiled these under
/// the one-DB-per-repo assumption, but they are not per-repo state:
///  * `content_revision` — digest over the WHOLE `main.files` (no `repo_id` filter);
///  * `fts_dirty` / `fts_source_revision` / `fts_synced_at_ms` — freshness of the ONE global
///    `chunk_fts` FTS5 index (never repo-scoped);
///  * `vector_int8_reencode_cursor` — resume marker for a walk over the WHOLE `chunk_embeddings`
///    table, beside an already-global done-gate.
///
/// Per-repo copies caused stale-dirty loops (a consolidated DB paying a full FTS rebuild forever
/// after a sibling synced) and cross-repo resume confusion. The runtime accessors write these to
/// the global `index_meta` (`self.meta` / `ai::set_meta`); this migration relocates any
/// pre-existing per-repo copy.
const V040_REPO_META_KEYS_TO_GLOBAL: &[&str] = &[
    "content_revision",
    "fts_dirty",
    "fts_source_revision",
    "fts_synced_at_ms",
    "vector_int8_reencode_cursor",
];

/// The subset of [`V040_REPO_META_KEYS_TO_GLOBAL`] that also appears in [`V039_INDEX_META_KEYS`],
/// so [`relocate_meta_keys`] must EXCLUDE them from V039's `index_meta → repo_meta` sweep (the
/// runtime now stores them globally in `index_meta`). The reencode cursor is reclassified too but
/// lives in `V039_RECONCILE_META_KEYS` (a different source table) AND, once global, sits in
/// `index_meta` where neither V039 sweep names it — so it needs no exclusion here; V040's move-back
/// handles a stale per-repo copy.
const RECLASSIFIED_GLOBAL_INDEX_META_KEYS: &[&str] =
    &["content_revision", "fts_dirty", "fts_source_revision", "fts_synced_at_ms"];

/// Move the listed keys OUT of the per-repo `repo_meta` and back into the GLOBAL `index_meta` — the
/// inverse of [`relocate_meta_keys`], correcting V039's over-relocation of global-infrastructure
/// state (see [`V040_REPO_META_KEYS_TO_GLOBAL`]).
///
/// IDEMPOTENT + MULTI-REPO-REPLAY-SAFE (the round-4 pattern): resolves NO `sole_repo_id`, so it
/// never hard-errors on a consolidated DB. `INSERT OR IGNORE` into the `key`-PK'd `index_meta`
/// keeps ONE global value — every repo computes the same content digest so their copies agree;
/// where a per-repo copy could differ (`fts_dirty`) the first row wins and the `DELETE` still
/// clears every copy, and the next FTS freshness check self-corrects the surviving global value.
/// The `value IS NOT NULL` guard skips a `repo_meta` NULL (its `value` is nullable;
/// `index_meta.value` is `NOT NULL`). On a re-apply the `repo_meta` rows are already gone, so both
/// statements are no-ops — the whole helper is safe to run on every `schema::apply`.
fn move_repo_meta_keys_to_global(conn: &Connection, keys: &[&str]) -> rusqlite::Result<()> {
    let in_list = keys.iter().map(|key| format!("'{key}'")).collect::<Vec<_>>().join(", ");
    conn.execute(
        &format!(
            "INSERT OR IGNORE INTO index_meta(key, value)
             SELECT key, value FROM repo_meta WHERE key IN ({in_list}) AND value IS NOT NULL"
        ),
        [],
    )?;
    conn.execute(&format!("DELETE FROM repo_meta WHERE key IN ({in_list})"), [])?;
    Ok(())
}

/// V040 (memory-sync phase A3): add `repo_id` scoping to the core tables that have no FK path to
/// `files` (they scope DIRECTLY; the `files`-reachable tables scope transitively through the
/// `files.repo_id` the scope view filters on). Per the disposition table:
///  * `files` — direct `repo_id`; UNIQUE becomes `(repo_id, path, commit_sha, worktree_id)`.
///  * `packages` — direct; UNIQUE becomes `(repo_id, manifest_dir, commit_sha, worktree_id)`.
///  * `logical_symbols`, `docs` — direct `repo_id` (add column; no key change).
///  * `parser_failures` — direct; PK becomes `(repo_id, path)` (was a bare autoincrement id, so
///    `remove_file_in_scope`'s path-only delete clobbered a sibling repo — inventory #12).
///  * `git_commits` — direct; PK becomes `(repo_id, hash)`; `commit_fts` external content follows
///    and `git_file_changes` gains `repo_id` + a composite `(repo_id, commit_hash)` FK.
///
/// Also reunites the two straggler active-model meta keys with their family in `repo_meta`.
///
/// The key/PK changes force table REBUILDS (SQLite can't alter a UNIQUE/PK in place). Modeled on
/// the V031 STRICT-rebuild recipe: `foreign_keys = OFF` OUTSIDE `BEGIN IMMEDIATE`,
/// RENAME→CREATE→copy→ DROP preserving rowids/ids, ROLLBACK on error, then `COMMIT; foreign_keys =
/// ON`. Every rebuild is wrapped in ONE transaction so a failure leaves the schema untouched;
/// `files.repo_id` existing is the all-or-nothing sentinel (atomic ⇒ present iff the whole V040
/// committed), so a re-apply (fresh DB already transformed, or `create_or_migrate`/`rebuild` on an
/// applied DB) short-circuits.
///
/// #51 GUARD: `commit_fts` is external-content over `git_commits`; after the git_commits rebuild its
/// stored rowids are stale, so it is resynced with the desync-safe `'rebuild'` command, NEVER a
/// `DELETE FROM commit_fts` (which corrupts a desynced external-content index).
pub fn apply_repo_id_core_scoping(
    conn: &Connection,
    hooks: &crate::hooks::MigrationHooks,
) -> rusqlite::Result<()> {
    // Reclassify the global-infrastructure keys V039 over-relocated to `repo_meta` back to the
    // GLOBAL `index_meta`. Runs BEFORE the `files.repo_id` short-circuit so it also corrects a DB a
    // PRIOR commit of THIS (unreleased) branch already migrated to V040 with those keys in
    // `repo_meta` — that DB's `files.repo_id` exists, so the short-circuit would otherwise skip it.
    // Idempotent + multi-repo-safe (resolves no `sole_repo_id`), so running it on every apply is a
    // no-op once the keys are global.
    move_repo_meta_keys_to_global(conn, V040_REPO_META_KEYS_TO_GLOBAL)?;

    // All-or-nothing sentinel: the rebuilds below run under one atomic transaction, so `files`
    // carrying `repo_id` means the whole migration already committed — a fresh-from-target DB or a
    // `create_or_migrate`/`rebuild` re-apply. Short-circuit before taking the write lock.
    if column_exists(conn, "files", "repo_id")? {
        return Ok(());
    }

    // The table rebuilds need FK enforcement OFF (they RENAME/DROP FK-referenced parents); a PRAGMA
    // toggle is a no-op inside a transaction, so set it BEFORE BEGIN (V031 recipe).
    conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        rebuild_files_table_with_repo_id(conn)?;
        rebuild_packages_table_with_repo_id(conn)?;
        // `logical_symbols` / `docs` are FK-less on their scoping key (no UNIQUE/PK change), so a
        // plain additive column suffices — no rebuild.
        add_column_if_missing(conn, "logical_symbols", "repo_id", REPO_ID_COLUMN_DEF)?;
        add_column_if_missing(conn, "docs", "repo_id", REPO_ID_COLUMN_DEF)?;
        rebuild_parser_failures_table_with_repo_id(conn)?;
        rebuild_git_commits_tables_with_repo_id(conn)?;
        // Re-point the placeholder-backfilled rows onto the sole repo that owns this DB. On an
        // already-adopted DB this is what keeps the rebuilt rows visible; on a not-yet-adopted DB
        // it is a no-op (see the helper). Runs inside the txn, atomic with the rebuilds.
        backfill_repo_id_to_sole_repo(conn)?;
        // Now that every `logical_symbols` row carries its final `repo_id`, migrate its
        // content-derived id to the new `repo_id`-folded derivation and re-point every reference
        // (memories, monikers, members), so pre-V040 memory/oracle handles survive the first
        // `rebuild_logical_symbols` after upgrade instead of dangling. FK enforcement is OFF for
        // the whole V040 transaction (set before BEGIN), which is exactly what the
        // parent-id remap needs. On a not-yet-adopted DB the rows carry the placeholder
        // here; `register_repo` runs the same realign again after adopting the real id
        // (idempotent — already-aligned rows are skipped).
        (hooks.realign_logical_symbol_ids)(conn)?;
        // Reunite the two active-model provenance stragglers with their family in `repo_meta`
        // (relocated under the sole repo via `relocate_meta_keys`' own `sole_repo_id` resolution;
        // `register_repo` keeps them there). Runs inside the txn so it is atomic with the rebuilds.
        relocate_meta_keys(conn, "index_meta", V040_INDEX_META_KEYS)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;");
        return result;
    }
    conn.execute_batch("COMMIT; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// Re-point every V040 direct-scoped table's placeholder-backfilled rows onto the sole repo that
/// owns this DB — the same established resolution [`relocate_meta_keys`] uses for the meta move
/// (and the V039 fix e2a2bc5 established).
///
/// The rebuilds / `add_column`s above stamp every existing row with the STATIC `__unassigned__`
/// column DEFAULT (a DEFAULT cannot be a runtime-resolved value). That is correct on a NOT-yet-
/// adopted DB — the sole repo IS the placeholder, so the rows already carry the right id and
/// [`super::register_repo`] re-points them at adoption. But on an ALREADY-ADOPTED DB (V038/V039 ran
/// `register_repo`, so `repos` holds the real id and NO placeholder), `register_repo` takes the
/// "already registered" fast path that never re-points — so the rebuilt rows would orphan under
/// `__unassigned__` and the active real-repo scope view would see an EMPTY index after upgrade.
/// Resolve the sole `repos` row (real when adopted, else the placeholder) via [`sole_repo_id`] and
/// UPDATE the placeholder rows onto it; skip the no-op churn when the DB is not yet adopted.
///
/// FK enforcement is OFF for the whole V040 transaction, so `git_commits`' `ON UPDATE CASCADE` does
/// NOT fire here — `git_file_changes` is re-pointed EXPLICITLY (unlike the runtime `register_repo`
/// path, which runs with FK ON and relies on that cascade, so it lists only `git_commits`).
fn backfill_repo_id_to_sole_repo(conn: &Connection) -> rusqlite::Result<()> {
    let target = sole_repo_id(conn)?;
    // Not adopted yet: rows already carry the placeholder; `register_repo` re-points at adoption.
    if target == rag_rat_base::repo_identity::LEGACY_REPO_ID {
        return Ok(());
    }
    for table in [
        "files",
        "packages",
        "logical_symbols",
        "docs",
        "parser_failures",
        "git_commits",
        "git_file_changes",
    ] {
        conn.execute(&format!("UPDATE {table} SET repo_id = ?1 WHERE repo_id = ?2"), [
            target.as_str(),
            rag_rat_base::repo_identity::LEGACY_REPO_ID,
        ])?;
    }
    Ok(())
}

/// Rebuild `files` with a leading `repo_id` column and the widened UNIQUE key. `files` is the
/// scoping ROOT and is FK-referenced by `chunks` / `symbols` / `edges_data` (all `REFERENCES
/// files(id)`), so the `id` values MUST be preserved — the rebuild copies them verbatim (the V008
/// `rebuild_files_table_for_commit_scopes` precedent). The full current column set (through V024's
/// `has_test_code`) is reproduced; `files` stays non-STRICT to match its baseline shape.
fn rebuild_files_table_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        -- Re-convergence: migrations run in AUTOCOMMIT, so a crash that killed a prior V040 pass
        -- mid-rebuild could leave the scratch table behind (the enclosing BEGIN/ROLLBACK normally
        -- prevents this, but a hard process kill bypasses a clean rollback). Drop it so the \
         rebuild
        -- restarts from a clean slate rather than failing on CREATE.
        DROP TABLE IF EXISTS files_new;
        CREATE TABLE files_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            kind TEXT NOT NULL,
            sha256 TEXT NOT NULL,
            modified_at_ms INTEGER NOT NULL,
            generated INTEGER NOT NULL DEFAULT 0,
            indexed_at_ms INTEGER NOT NULL,
            indexed_revision TEXT NOT NULL DEFAULT '',
            commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '',
            has_test_code INTEGER NOT NULL DEFAULT 0,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            UNIQUE(repo_id, path, commit_sha, worktree_id)
        );
        INSERT OR IGNORE INTO files_new(
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, commit_sha, worktree_id, has_test_code, repo_id
        )
        SELECT
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, commit_sha, worktree_id, has_test_code, '__unassigned__'
        FROM files;
        DROP TABLE files;
        ALTER TABLE files_new RENAME TO files;
        CREATE INDEX IF NOT EXISTS idx_files_language ON files(language);
        CREATE INDEX IF NOT EXISTS idx_files_commit_path ON files(commit_sha, path);
        CREATE INDEX IF NOT EXISTS idx_files_worktree_path ON files(worktree_id, path);
        ",
    )
}

/// V043 (phase A6): add `files.generation` and widen the UNIQUE to `(repo_id, path, commit_sha,
/// worktree_id, generation)`, so a full rebuild can STAGE a fresh generation of every file row
/// ALONGSIDE the live one (same `(repo_id, path, commit_sha, worktree_id)`, different `generation`)
/// and flip readers over atomically, instead of clearing-then-reinserting inside one long
/// write-locked transaction (spec §3.3 — bounded writer holds + reader consistency).
///
/// SENTINEL: `files.generation` present ⇒ the whole migration already committed (the rebuild below
/// is atomic), so a `create_or_migrate`/`rebuild`/`index --full` re-apply short-circuits before the
/// write lock — the V040/V042 recipe.
///
/// NO placeholder→sole-repo backfill (unlike V040–V042): `generation` is REPO-NEUTRAL. Every
/// pre-V043 row is the current live generation of its repo, and `DEFAULT 0` stamps them all 0 —
/// which is exactly the live generation a fresh index carries (`repo_meta[live_files_generation]`
/// absent ⇒ 0). So existing rows stay visible under the live-generation scope view with no per-repo
/// resolution, on an adopted or an un-adopted DB alike.
pub fn apply_files_generation(conn: &Connection) -> rusqlite::Result<()> {
    // All-or-nothing sentinel: the rebuild commits atomically, so `files.generation` present means
    // the whole migration already ran. Short-circuit before taking the write lock.
    if column_exists(conn, "files", "generation")? {
        return Ok(());
    }
    // The rebuild RENAMEs an FK-referenced parent (`chunks`/`symbols`/`edges_data` REFERENCE
    // files(id)); FK enforcement must be OFF, and a PRAGMA toggle is a no-op inside a transaction,
    // so set it BEFORE BEGIN (the V040 recipe).
    conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")?;
    let result = rebuild_files_table_with_generation(conn);
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;");
        return result;
    }
    conn.execute_batch("COMMIT; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// Rebuild `files` adding a trailing `generation INTEGER NOT NULL DEFAULT 0` column and the widened
/// `UNIQUE(repo_id, path, commit_sha, worktree_id, generation)`. Copies `id` verbatim (FK target of
/// `chunks`/`symbols`/`edges_data` — the V040 `rebuild_files_table_with_repo_id` precedent) and
/// stamps every existing row `generation = 0` (the live generation of a not-yet-restaged index).
/// `files` stays non-STRICT to match its baseline shape. Leading `DROP TABLE IF EXISTS files_new` =
/// torn-state re-convergence (a hard kill of a prior V043 pass could leave the scratch table
/// behind).
fn rebuild_files_table_with_generation(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS files_new;
        CREATE TABLE files_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            kind TEXT NOT NULL,
            sha256 TEXT NOT NULL,
            modified_at_ms INTEGER NOT NULL,
            generated INTEGER NOT NULL DEFAULT 0,
            indexed_at_ms INTEGER NOT NULL,
            indexed_revision TEXT NOT NULL DEFAULT '',
            commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '',
            has_test_code INTEGER NOT NULL DEFAULT 0,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            generation INTEGER NOT NULL DEFAULT 0,
            UNIQUE(repo_id, path, commit_sha, worktree_id, generation)
        );
        INSERT OR IGNORE INTO files_new(
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, commit_sha, worktree_id, has_test_code, repo_id, generation
        )
        SELECT
            id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms,
            indexed_revision, commit_sha, worktree_id, has_test_code, repo_id, 0
        FROM files;
        DROP TABLE files;
        ALTER TABLE files_new RENAME TO files;
        CREATE INDEX IF NOT EXISTS idx_files_language ON files(language);
        CREATE INDEX IF NOT EXISTS idx_files_commit_path ON files(commit_sha, path);
        CREATE INDEX IF NOT EXISTS idx_files_worktree_path ON files(worktree_id, path);
        ",
    )
}

/// Rebuild `packages` (STRICT) with a leading `repo_id` column and the widened UNIQUE key.
/// `packages` has no FK children (the file→package map is computed at load time, never persisted —
/// #106), so no id preservation is required, but the rows are copied to keep an already-populated
/// index intact through the migration.
fn rebuild_packages_table_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS packages_new;
        CREATE TABLE packages_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            manifest_dir TEXT NOT NULL,
            commit_sha TEXT NOT NULL DEFAULT '',
            worktree_id TEXT NOT NULL DEFAULT '',
            local_roots_json TEXT NOT NULL DEFAULT '[]',
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            UNIQUE(repo_id, manifest_dir, commit_sha, worktree_id)
        ) STRICT;
        INSERT OR IGNORE INTO packages_new(
            id, manifest_dir, commit_sha, worktree_id, local_roots_json, repo_id
        )
        SELECT id, manifest_dir, commit_sha, worktree_id, local_roots_json, '__unassigned__'
        FROM packages;
        DROP TABLE packages;
        ALTER TABLE packages_new RENAME TO packages;
        CREATE INDEX IF NOT EXISTS idx_packages_scope ON packages(commit_sha, worktree_id);
        ",
    )
}

/// Rebuild `parser_failures` with PK `(repo_id, path)` (was a bare autoincrement `id`). The old
/// shape allowed multiple rows per path and `remove_file_in_scope` deleted by bare path —
/// clobbering a sibling repo's failure once repos share a DB (inventory #12). The new PK collapses
/// to one row per `(repo_id, path)`; `INSERT OR IGNORE` dedupes any legacy multi-row-per-path data.
/// Nothing FK-references `parser_failures`, so dropping the `id` column is safe. Rebuilt STRICT.
fn rebuild_parser_failures_table_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS parser_failures_new;
        CREATE TABLE parser_failures_new(
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            path TEXT NOT NULL,
            language TEXT NOT NULL,
            message TEXT NOT NULL,
            PRIMARY KEY(repo_id, path)
        ) STRICT;
        INSERT OR IGNORE INTO parser_failures_new(repo_id, path, language, message)
        SELECT '__unassigned__', path, language, message FROM parser_failures;
        DROP TABLE parser_failures;
        ALTER TABLE parser_failures_new RENAME TO parser_failures;
        ",
    )
}

/// Rebuild `git_commits` with PK `(repo_id, hash)` and `git_file_changes` with `repo_id` + a
/// composite `(repo_id, commit_hash)` FK to it (`ON DELETE CASCADE ON UPDATE CASCADE` — the UPDATE
/// cascade is what lets `register_repo` adoption re-point `git_commits.repo_id` and carry the
/// changes along). `git_commits` rowids are PRESERVED (copied explicitly) so the `commit_fts`
/// external-content rowid mapping stays valid; it is then resynced with the desync-safe `'rebuild'`
/// (#51 — never `DELETE FROM commit_fts`). These two tables were repo-GLOBAL before; scoping them
/// stops a `git history` reindex of one repo from wiping another's commits.
fn rebuild_git_commits_tables_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS git_commits_new;
        DROP TABLE IF EXISTS git_file_changes_new;
        CREATE TABLE git_commits_new(
            hash TEXT NOT NULL,
            author_name TEXT NOT NULL,
            author_email TEXT NOT NULL,
            authored_at_s INTEGER NOT NULL,
            committed_at_s INTEGER NOT NULL,
            subject TEXT NOT NULL,
            body TEXT NOT NULL,
            changed_file_count INTEGER NOT NULL DEFAULT 0,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            PRIMARY KEY(repo_id, hash)
        ) STRICT;
        INSERT OR IGNORE INTO git_commits_new(
            rowid, hash, author_name, author_email, authored_at_s, committed_at_s,
            subject, body, changed_file_count, repo_id
        )
        SELECT
            rowid, hash, author_name, author_email, authored_at_s, committed_at_s,
            subject, body, changed_file_count, '__unassigned__'
        FROM git_commits;
        DROP TABLE git_commits;
        ALTER TABLE git_commits_new RENAME TO git_commits;

        CREATE TABLE git_file_changes_new(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            commit_hash TEXT NOT NULL,
            path TEXT NOT NULL,
            additions INTEGER,
            deletions INTEGER,
            change_kind TEXT NOT NULL DEFAULT 'modified',
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            FOREIGN KEY(repo_id, commit_hash)
                REFERENCES git_commits(repo_id, hash) ON DELETE CASCADE ON UPDATE CASCADE
        );
        INSERT OR IGNORE INTO git_file_changes_new(
            id, commit_hash, path, additions, deletions, change_kind, repo_id
        )
        SELECT id, commit_hash, path, additions, deletions, change_kind, '__unassigned__'
        FROM git_file_changes;
        DROP TABLE git_file_changes;
        ALTER TABLE git_file_changes_new RENAME TO git_file_changes;
        CREATE INDEX IF NOT EXISTS idx_git_file_changes_path ON git_file_changes(path);
        CREATE INDEX IF NOT EXISTS idx_git_file_changes_commit ON git_file_changes(commit_hash);

        -- Resync the external-content FTS after the content-table rebuild (#51: 'rebuild', never a
        -- DELETE, on a possibly-desynced external-content index).
        INSERT INTO commit_fts(commit_fts) VALUES('rebuild');
        ",
    )
}

/// The seven GitHub papertrail tables V041 (phase A4) gives a direct `repo_id` column. Order is
/// stable so the adoption re-point loop and this migration agree; `github_fts` is NOT here — it is
/// the standalone FTS mirror, rebuilt separately by [`rebuild_github_fts_with_repo_id`].
const V041_GITHUB_SCOPED_TABLES: &[&str] = &[
    "github_refs",
    "github_issues",
    "github_comments",
    "github_pull_requests",
    "github_reviews",
    "github_review_comments",
    "github_ref_sync",
];

/// V041 (memory-sync phase A4): repo-scope the GitHub papertrail cache so a lexical/papertrail
/// query in a consolidated DB never surfaces a sibling repo's refs or issues. The seven
/// [`V041_GITHUB_SCOPED_TABLES`] were repo-GLOBAL before; each gains a `repo_id` column
/// ([`REPO_ID_COLUMN_DEF`] — existing rows backfill to the placeholder that `register_repo`
/// re-points at adoption, the A1/A2/A3 pattern), and the standalone `github_fts` gains a `repo_id
/// UNINDEXED` column via a REBUILD (its FTS5 columns can't be ALTERed).
///
/// SCOPE (deliberate): the base tables gain the column but keep their existing keys
/// (`github_issues`/`github_pull_requests` `UNIQUE(owner, repo, number)`; `github_ref_sync`'s PK;
/// the id-keyed caches). Phase A keeps ONE repo per DB (`register_repo` refuses a second real repo
/// until A7), so those keys stay correct; widening them to include `repo_id` for the eventual
/// multi-repo DB is an A7 concern. Reads scope by the connection's active `repo_id`; writes stamp
/// it.
///
/// TORN-STATE / RE-APPLY: the whole migration self-wraps in ONE `BEGIN IMMEDIATE ... COMMIT` (the
/// ladder runs apply fns in AUTOCOMMIT), so the base-table columns, the `github_fts` rebuild, AND
/// the already-adopted-DB backfill commit together — the `github_fts.repo_id` sentinel flips only
/// once every step (backfill included) has landed. `add_column_if_missing` and the rebuild's
/// leading `DROP ... IF EXISTS` keep a re-run after a torn intermediate idempotent. All-or-nothing
/// sentinel: `github_fts` carrying `repo_id` means the whole migration already committed, so a
/// fresh-from-target DB or a `create_or_migrate` / `rebuild` re-apply short-circuits before
/// touching anything (and never resolves `sole_repo_id`, keeping the ladder replay-safe on a
/// consolidated DB).
pub fn apply_github_repo_id_scoping(conn: &Connection) -> rusqlite::Result<()> {
    // Post-V060 the legacy github_* tables no longer exist AT ALL: the baseline creates the
    // provider-neutral papertrail_* tables instead, so on a fresh DB (or any DB the V060
    // normalization already converged) there is nothing for this migration to scope — the ladder
    // still replays it in order, and it must no-op instead of ALTERing absent tables. Only
    // reachable-legacy DBs (github_fts present, created by a pre-V060 baseline) take the body.
    if !sqlite_object_exists(conn, "table", "github_fts")? {
        return Ok(());
    }
    if column_exists(conn, "github_fts", "repo_id")? {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        // Each base-table column is independent additive DDL (one atomic ALTER).
        for table in V041_GITHUB_SCOPED_TABLES {
            add_column_if_missing(conn, table, "repo_id", REPO_ID_COLUMN_DEF)?;
        }
        rebuild_github_fts_with_repo_id(conn)?;
        // P1 (the V039/V040 class): the static `DEFAULT '__unassigned__'` stamps existing rows the
        // placeholder, and on an ALREADY-ADOPTED DB `register_repo`'s fast path never re-points —
        // so scoped papertrail reads would return NOTHING for the real repo until the next github
        // sync. Resolve the sole `repos` row and re-point the placeholder rows onto it (no-op on an
        // un-adopted DB, where the rows correctly wait for `register_repo` to adopt them).
        backfill_github_repo_id_to_sole_repo(conn)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
        return result;
    }
    conn.execute_batch("COMMIT;")
}

/// Rebuild the standalone (own-content) `github_fts` with an added `repo_id UNINDEXED` column via
/// the create-new / copy / drop / rename recipe. Its rows are copied stamped the placeholder; the
/// caller's [`backfill_github_repo_id_to_sole_repo`] then re-points them onto the real id on an
/// adopted DB.
///
/// Runs INSIDE the caller's transaction ([`apply_github_repo_id_scoping`] wraps the whole V041
/// migration in one `BEGIN IMMEDIATE`), so it opens no txn of its own. The leading `DROP TABLE IF
/// EXISTS github_fts_new` drops a hard-kill scratch artifact (which bypasses rollback) so the
/// rebuild re-converges rather than failing on CREATE — the V040 recipe. `github_fts` has no FK, so
/// no `foreign_keys` toggle is needed.
fn rebuild_github_fts_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS github_fts_new;
        CREATE VIRTUAL TABLE github_fts_new USING fts5(
            owner,
            repo,
            number UNINDEXED,
            item_kind UNINDEXED,
            item_id UNINDEXED,
            url UNINDEXED,
            title,
            body,
            classification,
            repo_id UNINDEXED,
            tokenize='porter'
        );
        INSERT INTO github_fts_new(
            owner, repo, number, item_kind, item_id, url, title, body, classification, repo_id
        )
        SELECT owner, repo, number, item_kind, item_id, url, title, body, classification,
               '__unassigned__'
        FROM github_fts;
        DROP TABLE github_fts;
        ALTER TABLE github_fts_new RENAME TO github_fts;
        ",
    )
}

/// P1 backfill (the V040 [`backfill_repo_id_to_sole_repo`] pattern, github edition): re-point every
/// V041 github row from the placeholder onto the sole `repos` id when the DB is ALREADY ADOPTED, so
/// a scoped papertrail read sees this repo's cached refs/issues immediately after the upgrade
/// rather than only after the next sync. No-op on an un-adopted DB (`sole_repo_id` == the
/// placeholder — the rows correctly wait for `register_repo` to adopt them). Runs inside the
/// caller's V041 txn, so it is atomic with the sentinel-setting FTS rebuild. The sentinel
/// short-circuit keeps a replay off this path entirely; the explicit `repos`-cardinality gate
/// below additionally makes the FIRST apply safe on a CONSOLIDATED DB (leave-at-placeholder
/// instead of `sole_repo_id`'s one-row hard error aborting the upgrade).
fn backfill_github_repo_id_to_sole_repo(conn: &Connection) -> rusqlite::Result<()> {
    // Cardinality gate: the backfill re-points onto THE sole owner of a single-repo DB, and only
    // that shape has one. A CONSOLIDATED DB (multiple `repos` rows) that reaches V041 without the
    // sentinel must not abort the forward-migration on `sole_repo_id`'s one-row hard error — there
    // is no single correct owner to pick. Invariant: on `!= 1` repos rows the github rows stay
    // under the placeholder, which is safe because (a) the papertrail is a refetchable CACHE, not
    // authored data; (b) every scoped reader filters `repo_id = <active>`, so placeholder rows are
    // simply invisible — never misattributed; and (c) the github writers RECLAIM stranded rows by
    // UPSERT: the tables keep natural keys WITHOUT `repo_id` (the phase-A deviation), so a
    // placeholder row OCCUPIES its key and a bare `INSERT OR IGNORE` could never repopulate it —
    // instead each writer's `ON CONFLICT ... DO UPDATE` (or `OR REPLACE` on the id-keyed caches)
    // re-stamps `repo_id` and refreshes the content on the next sync touching the key, and the
    // sync-tail `rebuild_fts` re-derives the mirror accordingly (the upsert-reclaim invariant in
    // `github/store.rs`).
    let repos_rows: i64 = conn.query_row("SELECT COUNT(*) FROM repos", [], |row| row.get(0))?;
    if repos_rows != 1 {
        return Ok(());
    }
    let target = sole_repo_id(conn)?;
    if target == rag_rat_base::repo_identity::LEGACY_REPO_ID {
        return Ok(());
    }
    for table in V041_GITHUB_SCOPED_TABLES {
        conn.execute(&format!("UPDATE {table} SET repo_id = ?1 WHERE repo_id = ?2"), [
            target.as_str(),
            rag_rat_base::repo_identity::LEGACY_REPO_ID,
        ])?;
    }
    // The own-content FTS mirror carries its own `repo_id UNINDEXED` value; re-point it in place
    // too.
    conn.execute("UPDATE github_fts SET repo_id = ?1 WHERE repo_id = ?2", [
        target.as_str(),
        rag_rat_base::repo_identity::LEGACY_REPO_ID,
    ])?;
    Ok(())
}

// ============================================================================================
// Memory-sync phase A5 (periphery scoping) — V042.
//
// Registered in `ADDITIVE_MIGRATIONS` as V042 (id `042_repo_id_periphery_scoping`): the whole
// schema change is `apply_repo_id_periphery_scoping` below, shaped like every other ladder step
// (idempotent, atomic, torn-state re-convergent, placeholder-backfilled). It stacks on V041 (the
// GitHub papertrail scoping); the two were authored in parallel workstreams and consolidated here.
//
// GATING (why the queries still probe): every periphery query sweep gates its `repo_id` predicate
// on [`super::periphery_repo_scope`] — column present ⇒ scope by the active repo, column absent ⇒
// the original unscoped SQL. On a normal open `schema::apply` runs the full ladder including V042,
// so the column is always present and the scoped path always taken. The absent branch is the
// defensive path for a raw connection that never ran the ladder (and the pre-migration schema in
// forward-migration bootstrap tests): it degrades to the pre-A5 repo-global behavior instead of
// referencing a column that does not exist. Keeping the probe is what lets raw-connection callers
// and partial-schema fixtures share these code paths without a separate unscoped variant.
// ============================================================================================

/// The periphery tables A5 scopes DIRECTLY by `repo_id` via an ADDITIVE `repo_id` column (no PK /
/// UNIQUE change — [`add_column_if_missing`] suffices, idempotent + AUTOCOMMIT-safe). Adoption
/// re-points their placeholder rows (see `registry::A5_PERIPHERY_DIRECT_SCOPED_TABLES`).
const A5_ADDITIVE_SCOPED_TABLES: &[&str] = &[
    // Per-repo generation counter (the generation integer stays globally unique — see the apply
    // fn — so the transitive `clone_edges`/`clone_subblock_postings` need no `repo_id`).
    "clone_graph_generations",
    // Oracle run log; the read "key" (repo_id, tool, tool_version, commit_sha, worktree_id) is a
    // query predicate, not a UNIQUE, so a plain column is enough.
    "oracle_runs",
    // Reconcile run log (no UNIQUE); the "latest attempt" read scopes by repo_id.
    "reconcile_attempts",
    // Memories + per-binding repo_id (spec §4.5 cross-repo bindings default to the parent memory's
    // repo; the PK id TEXT / (memory_id, binding_kind, binding_id) are unchanged).
    "repo_memories",
    "repo_memory_bindings",
];

/// V042 (memory-sync phase A5, see the block comment above): add `repo_id`
/// scoping to the clone / oracle / reconcile / memory PERIPHERY tables — every direct-scoped table
/// left after A3's core sweep (per the plan's disposition table). Two shapes:
///
///  * ADDITIVE column ([`A5_ADDITIVE_SCOPED_TABLES`]): `clone_graph_generations`, `oracle_runs`,
///    `reconcile_attempts`, `repo_memories`, `repo_memory_bindings` — no key change, so
///    `add_column_if_missing` (self-guarding) is all that is needed.
///  * KEY REBUILD (`repo_id` joins the PK / UNIQUE, so SQLite forces a table rebuild):
///    `clone_token_df` (PK `(repo_id, normalizer_kind, token_hash)` — df must not pool across
///    repos), `clone_refinements` (PK `(repo_id, class_key)` — content class-keys collide across
///    repos), `edge_oracle` (content-key PK gains a leading `repo_id`), `logical_symbol_monikers`
///    (PK `(repo_id, logical_symbol_id, tool)`), `dream_findings` (UNIQUE `(repo_id, kind, subject,
///    claim_hash)`).
///  * STANDALONE FTS rebuild: `repo_memory_fts` gains `repo_id UNINDEXED` and a mandatory filter —
///    rebuilt from `repo_memories` (the same content `upsert_memory_fts` writes), so `repo_id`
///    comes from each memory's freshly-added column.
///
/// `clone_edges` / `clone_subblock_postings` are NOT scoped directly (disposition: transitive via
/// the generations FK). The generation integer stays GLOBALLY unique (allocated `MAX(generation) +
/// 1` over ALL repos), so a `build_generation` value belongs to exactly one repo and those children
/// are scoped for free; only the generation SWEEPS (`complete_generation`, the Building cleanup,
/// the postings invalidation) must add a `repo_id` predicate so a repo's precompute never deletes a
/// sibling's generations. `symbol_fingerprints` / `symbol_token_postings` stay `symbol_id`-keyed
/// (transitive via `symbols -> files.repo_id`) — the "NEVER add scope columns here" rule in
/// [`CLONE_FINGERPRINT_DDL`] is about the COMMIT/WORKTREE scope axis; `repo_id` is a different
/// (cross-repo) axis and only `clone_token_df` / `clone_refinements` there take it.
///
/// MECHANICS (identical to V040): the key rebuilds RENAME/DROP tables, which needs FK enforcement
/// OFF; a PRAGMA toggle is a no-op inside a transaction, so it is set BEFORE `BEGIN IMMEDIATE`. The
/// whole migration runs under ONE atomic transaction (the ladder runs apply fns in AUTOCOMMIT, so a
/// multi-statement rebuild must self-wrap), and `repo_memories.repo_id` existing is the
/// all-or-nothing sentinel: because every statement commits together, that column is present iff
/// the whole migration committed, so a re-apply (fresh DB already transformed, or an `index --full`
/// re-run once this is registered) short-circuits before taking the write lock. Each rebuild starts
/// `DROP TABLE IF EXISTS <scratch>_new` so a prior pass killed mid-rebuild (bypassing a clean
/// rollback) re-converges from a clean slate rather than failing on `CREATE`.
pub fn apply_repo_id_periphery_scoping(
    conn: &Connection,
    hooks: &crate::hooks::MigrationHooks,
) -> rusqlite::Result<()> {
    // All-or-nothing sentinel (see the doc comment): everything below commits atomically, so
    // `repo_memories.repo_id` present means the whole migration already ran.
    if column_exists(conn, "repo_memories", "repo_id")? {
        return Ok(());
    }

    conn.execute_batch("PRAGMA foreign_keys = OFF; BEGIN IMMEDIATE;")?;
    let result = (|| -> rusqlite::Result<()> {
        // --- Additive columns (no key change): idempotent, placeholder-backfilled. ---
        for table in A5_ADDITIVE_SCOPED_TABLES {
            add_column_if_missing(conn, table, "repo_id", REPO_ID_COLUMN_DEF)?;
        }

        // --- Key rebuilds (repo_id joins the PK / UNIQUE). ---
        rebuild_clone_token_df_with_repo_id(conn)?;
        rebuild_clone_refinements_with_repo_id(conn)?;
        rebuild_edge_oracle_with_repo_id(conn)?;
        rebuild_logical_symbol_monikers_with_repo_id(conn)?;
        rebuild_dream_findings_with_repo_id(conn)?;

        // --- Standalone FTS rebuild (repo_id UNINDEXED), rebuilt from `repo_memories`. ---
        rebuild_repo_memory_fts_with_repo_id(conn)?;
        // P1 (the V039/V040 class): the additive columns' static `DEFAULT '__unassigned__'`, the
        // rebuilds' literal `'__unassigned__'` copy, and the FTS repopulated from those rows all
        // stamp the placeholder — and on an ALREADY-ADOPTED DB `register_repo`'s fast path never
        // re-points, so the scoped periphery reads (clones / oracle / memories) would miss this
        // repo's rows. Resolve the sole `repos` row and re-point them onto it (no-op on an
        // un-adopted DB, where the rows correctly wait for `register_repo`). Atomic with the
        // sentinel-setting steps because it runs inside this same txn.
        backfill_periphery_repo_id_to_sole_repo(conn)?;
        // Finding ids now FOLD `repo_id` (`dream::repo_folded_finding_id` — the stable_id
        // precedent), so re-derive every persisted id under its post-backfill `repo_id`. Runs
        // AFTER the backfill so an adopted DB's ids fold the real repo, not the placeholder.
        // `superseded_by` (the only persisted reference to a finding id, in-table) is remapped by
        // the helper. Replay-safe: the sentinel short-circuits a re-apply, and the re-derivation
        // is idempotent anyway.
        (hooks.rederive_dream_finding_ids)(conn)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys = ON;");
        return result;
    }
    conn.execute_batch("COMMIT; PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// P1 backfill (the V040 [`backfill_repo_id_to_sole_repo`] pattern, periphery edition): re-point
/// every V042 periphery row — the additive-column tables, the PK-rebuilt tables, AND the
/// `repo_memory_fts` mirror — from the placeholder onto the sole `repos` id when the DB is ALREADY
/// ADOPTED, so the scoped clone / oracle / memory reads see this repo's rows immediately after the
/// upgrade. No-op on an un-adopted DB (`sole_repo_id` == the placeholder — the rows correctly wait
/// for `register_repo`). Runs inside the V042 txn (FK OFF), so it is atomic with the sentinel. The
/// sentinel short-circuit keeps a replay off this path entirely; the explicit `repos`-cardinality
/// gate below additionally makes the FIRST apply safe on a CONSOLIDATED DB (leave-at-placeholder
/// instead of `sole_repo_id`'s one-row hard error aborting the upgrade). The table set mirrors
/// `A5_PERIPHERY_DIRECT_SCOPED_TABLES` (registry.rs), the same rows adoption re-points.
fn backfill_periphery_repo_id_to_sole_repo(conn: &Connection) -> rusqlite::Result<()> {
    // Cardinality gate (the V041 github-backfill twin): on `!= 1` repos rows there is no single
    // owner to re-point onto, so the periphery rows stay under the placeholder rather than
    // aborting the forward-migration. One nuance vs the github edition: placeholder-stranded
    // `repo_memories` would be USER-AUTHORED data invisible to scoped reads, not a refetchable
    // cache. That state is only reachable by hand-driving `register_repo` pre-consolidation
    // (phase A refuses a second real repo; the real multi-repo entry path arrives with the
    // consolidate importer, which attributes memories at import) — and `memory doctor` surfaces
    // any placeholder-scoped memories (one `placeholder_repo` entry each, `doctor_report`) so
    // they are visible, not silently lost. The clone / oracle / reconcile tables are derived
    // caches the next rebuild / oracle run / reconcile re-populates under the proper stamp.
    //
    // No github-style RECLAIM is needed here (audited per table): unlike the V041 github tables,
    // whose natural keys exclude `repo_id` and whose stranded rows therefore OCCUPY the key the
    // next sync writes to (see the upsert-reclaim invariant in `github/store.rs`), every V042
    // periphery key either LEADS with `repo_id` (clone_token_df, clone_refinements, edge_oracle,
    // logical_symbol_monikers, dream_findings' UNIQUE), is an append-only autoincrement log
    // (oracle_runs, reconcile_attempts), a fresh generated id (repo_memories + its bindings/fts,
    // keyed by the repo-unique memory id), or a globally-unique counter (clone_graph_generations,
    // allocated MAX(generation)+1 across ALL repos). New writes under the real repo can never
    // conflict with a placeholder row — stranded rows merely LINGER invisibly until a later gc
    // sweep reclaims the space (acceptable; nothing blocks repopulation).
    let repos_rows: i64 = conn.query_row("SELECT COUNT(*) FROM repos", [], |row| row.get(0))?;
    if repos_rows != 1 {
        return Ok(());
    }
    let target = sole_repo_id(conn)?;
    if target == rag_rat_base::repo_identity::LEGACY_REPO_ID {
        return Ok(());
    }
    for table in [
        "clone_graph_generations",
        "clone_token_df",
        "clone_refinements",
        "oracle_runs",
        "edge_oracle",
        "logical_symbol_monikers",
        "reconcile_attempts",
        "dream_findings",
        "repo_memories",
        "repo_memory_bindings",
        "repo_memory_fts",
    ] {
        conn.execute(&format!("UPDATE {table} SET repo_id = ?1 WHERE repo_id = ?2"), [
            target.as_str(),
            rag_rat_base::repo_identity::LEGACY_REPO_ID,
        ])?;
    }
    Ok(())
}

/// Rebuild `clone_token_df` with a leading `repo_id` in the PK so document-frequency stats never
/// pool across repos (a shared df corrupts the SourcererCC selectivity ordering for both repos).
/// No FK children; STRICT preserved.
fn rebuild_clone_token_df_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS clone_token_df_new;
        CREATE TABLE clone_token_df_new(
            repo_id         TEXT    NOT NULL DEFAULT '__unassigned__',
            normalizer_kind TEXT    NOT NULL,
            token_hash      INTEGER NOT NULL,
            df              INTEGER NOT NULL,
            PRIMARY KEY (repo_id, normalizer_kind, token_hash)
        ) STRICT;
        INSERT OR IGNORE INTO clone_token_df_new(repo_id, normalizer_kind, token_hash, df)
        SELECT '__unassigned__', normalizer_kind, token_hash, df FROM clone_token_df;
        DROP TABLE clone_token_df;
        ALTER TABLE clone_token_df_new RENAME TO clone_token_df;
        ",
    )
}

/// Rebuild `clone_refinements` with a leading `repo_id` in the PK — `class_key` is content-derived
/// (normalized code shape), so two repos with the same clone shape would otherwise clobber each
/// other's cached refinement. STRICT preserved; the full V030 column set (incl. `lcs_sampled`) is
/// reproduced.
fn rebuild_clone_refinements_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS clone_refinements_new;
        CREATE TABLE clone_refinements_new(
            repo_id                 TEXT    NOT NULL DEFAULT '__unassigned__',
            class_key               TEXT    NOT NULL,
            language                TEXT    NOT NULL,
            refine_mode             TEXT    NOT NULL,
            template                TEXT    NOT NULL,
            variation_points_json   TEXT    NOT NULL CHECK (json_valid(variation_points_json)),
            proposed_signature_json TEXT    NOT NULL CHECK (json_valid(proposed_signature_json)),
            confidence              TEXT    NOT NULL,
            anti_unify_coverage     REAL    NOT NULL,
            lcs_ratio               REAL    NOT NULL,
            refactorability         REAL    NOT NULL,
            norm_version            INTEGER NOT NULL,
            alignment_version       INTEGER NOT NULL,
            created_at_ms           INTEGER NOT NULL,
            lcs_sampled             INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (repo_id, class_key)
        ) STRICT;
        INSERT OR IGNORE INTO clone_refinements_new(
            repo_id, class_key, language, refine_mode, template, variation_points_json,
            proposed_signature_json, confidence, anti_unify_coverage, lcs_ratio, refactorability,
            norm_version, alignment_version, created_at_ms, lcs_sampled
        )
        SELECT
            '__unassigned__', class_key, language, refine_mode, template, variation_points_json,
            proposed_signature_json, confidence, anti_unify_coverage, lcs_ratio, refactorability,
            norm_version, alignment_version, created_at_ms, lcs_sampled
        FROM clone_refinements;
        DROP TABLE clone_refinements;
        ALTER TABLE clone_refinements_new RENAME TO clone_refinements;
        ",
    )
}

/// Rebuild `edge_oracle` with a leading `repo_id` in the content-key PK. It stays content-anchored
/// with NO FK to `edges_data` (the #248 rule — the read join to live edges filters dangling rows),
/// so prepending `repo_id` is a pure PK-widening rebuild; the three indexes are recreated. STRICT
/// preserved. A cross-repo content-key collision (same source span in two repos) would otherwise
/// surface one repo's verdict for the other; the scoped reads (store.rs) filter `repo_id`.
fn rebuild_edge_oracle_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS edge_oracle_new;
        CREATE TABLE edge_oracle_new(
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            source_path TEXT NOT NULL,
            source_start_byte INTEGER NOT NULL,
            source_end_byte INTEGER NOT NULL,
            callee_start_byte INTEGER NOT NULL,
            callee_end_byte INTEGER NOT NULL,
            edge_kind TEXT NOT NULL,
            file_sha TEXT NOT NULL,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            resolved_symbol_id INTEGER,
            scip_symbol TEXT NOT NULL,
            kind TEXT NOT NULL,
            computed_at INTEGER NOT NULL,
            PRIMARY KEY(
                repo_id, tool, tool_version, source_path,
                source_start_byte, source_end_byte,
                callee_start_byte, callee_end_byte, edge_kind
            )
        ) STRICT;
        INSERT OR IGNORE INTO edge_oracle_new(
            repo_id, source_path, source_start_byte, source_end_byte, callee_start_byte,
            callee_end_byte, edge_kind, file_sha, tool, tool_version, resolved_symbol_id,
            scip_symbol, kind, computed_at
        )
        SELECT
            '__unassigned__', source_path, source_start_byte, source_end_byte, callee_start_byte,
            callee_end_byte, edge_kind, file_sha, tool, tool_version, resolved_symbol_id,
            scip_symbol, kind, computed_at
        FROM edge_oracle;
        DROP TABLE edge_oracle;
        ALTER TABLE edge_oracle_new RENAME TO edge_oracle;
        CREATE INDEX IF NOT EXISTS idx_edge_oracle_staleness
            ON edge_oracle(file_sha, tool, tool_version);
        CREATE INDEX IF NOT EXISTS idx_edge_oracle_symbol
            ON edge_oracle(resolved_symbol_id);
        CREATE INDEX IF NOT EXISTS idx_edge_oracle_anchor
            ON edge_oracle(source_path, callee_start_byte, callee_end_byte, edge_kind);
        ",
    )
}

/// Rebuild `logical_symbol_monikers` with a leading `repo_id` in the PK. The id is content-derived
/// (`LogicalSymbolKey::stable_id`) so it collides across repos; NO FK to `logical_symbols` (the #70
/// rule — reads join live logical symbols). The moniker index is recreated. STRICT preserved.
fn rebuild_logical_symbol_monikers_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS logical_symbol_monikers_new;
        CREATE TABLE logical_symbol_monikers_new(
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            logical_symbol_id INTEGER NOT NULL,
            tool TEXT NOT NULL,
            tool_version TEXT NOT NULL,
            moniker TEXT NOT NULL,
            computed_at INTEGER NOT NULL,
            PRIMARY KEY(repo_id, logical_symbol_id, tool)
        ) STRICT;
        INSERT OR IGNORE INTO logical_symbol_monikers_new(
            repo_id, logical_symbol_id, tool, tool_version, moniker, computed_at
        )
        SELECT '__unassigned__', logical_symbol_id, tool, tool_version, moniker, computed_at
        FROM logical_symbol_monikers;
        DROP TABLE logical_symbol_monikers;
        ALTER TABLE logical_symbol_monikers_new RENAME TO logical_symbol_monikers;
        CREATE INDEX IF NOT EXISTS idx_logical_symbol_monikers_moniker
            ON logical_symbol_monikers(moniker, tool);
        ",
    )
}

/// Rebuild `dream_findings` with `repo_id` in the UNIQUE `(repo_id, kind, subject, claim_hash)` —
/// the dream worklist's identity key is content-derived (subject + claim hash of a memory-anchored
/// finding), so it must not merge two repos' findings. The `id TEXT PRIMARY KEY` is unchanged; both
/// indexes are recreated. STRICT preserved.
fn rebuild_dream_findings_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS dream_findings_new;
        CREATE TABLE dream_findings_new(
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            subject TEXT NOT NULL,
            claim_hash TEXT NOT NULL,
            evidence TEXT NOT NULL,
            base_rank REAL NOT NULL,
            status TEXT NOT NULL DEFAULT 'open',
            superseded_by TEXT,
            first_seen_at_ms INTEGER NOT NULL,
            last_seen_at_ms INTEGER NOT NULL,
            reviewed_at_ms INTEGER,
            repo_id TEXT NOT NULL DEFAULT '__unassigned__',
            UNIQUE(repo_id, kind, subject, claim_hash)
        ) STRICT;
        INSERT OR IGNORE INTO dream_findings_new(
            id, kind, subject, claim_hash, evidence, base_rank, status, superseded_by,
            first_seen_at_ms, last_seen_at_ms, reviewed_at_ms, repo_id
        )
        SELECT
            id, kind, subject, claim_hash, evidence, base_rank, status, superseded_by,
            first_seen_at_ms, last_seen_at_ms, reviewed_at_ms, '__unassigned__'
        FROM dream_findings;
        DROP TABLE dream_findings;
        ALTER TABLE dream_findings_new RENAME TO dream_findings;
        CREATE INDEX IF NOT EXISTS idx_dream_findings_status ON dream_findings(status);
        CREATE INDEX IF NOT EXISTS idx_dream_findings_subject ON dream_findings(kind, subject);
        ",
    )
}

/// Rebuild the standalone `repo_memory_fts` with a leading `repo_id UNINDEXED` column and
/// repopulate it from `repo_memories` (the same title/body/kind/tags content `upsert_memory_fts`
/// writes, tags space-joined). Rebuilding from source — rather than copying the old FTS rows — lets
/// `repo_id` come from each memory's freshly-added column and needs no FTS `RENAME` (which has
/// historically been fragile on shadow tables); the DROP + CREATE keeps the canonical table name.
/// `memory_search` then filters `repo_id` after the MATCH.
pub fn rebuild_repo_memory_fts_with_repo_id(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS repo_memory_fts;
        CREATE VIRTUAL TABLE repo_memory_fts USING fts5(
            repo_id UNINDEXED,
            memory_id UNINDEXED,
            title,
            body,
            kind,
            tags,
            tokenize='porter'
        );
        INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
        SELECT
            m.repo_id, m.id, m.title, m.body, m.kind,
            COALESCE(
                (SELECT group_concat(t.tag, ' ')
                 FROM repo_memory_tags t WHERE t.memory_id = m.id),
                ''
            )
        FROM repo_memories m;
        ",
    )
}
