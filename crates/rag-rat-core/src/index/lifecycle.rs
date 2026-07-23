use rag_rat_db::meta::{read_meta, repo_meta};
use rag_rat_db::schema;
use rag_rat_papertrail as papertrail;

use super::*;

/// How long an `open` waits for the index write lock before giving up on auto-migrating a schema
/// that lags this binary. Generous — a concurrent migrator (or a watcher maintenance pass holding
/// the lock) finishes well within it; a timeout surfaces an explicit error, never a silent
/// half-open.
const SCHEMA_MIGRATE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How a config-less open resolves its repo scope — the two callers of
/// [`IndexDatabase::open_bare`] want opposite postures on a multi-repo DB.
pub(super) enum BareOpenMode {
    /// A genuinely CONFIG-LESS open (`IndexDatabase::open`: the MCP `call_tool(database, …)` path,
    /// tests, tooling): scope stays the sole repo for the connection's lifetime, so a multi-repo
    /// DB is REFUSED up front (A7) and the graph/generated-flags heal runs for the sole repo.
    ConfigLess,
    /// The incremental pass's low-level first step: `adopt_repo_from_config` + `set_context`
    /// follow immediately, so the multi-repo refusal must NOT fire (the config supplies the scope)
    /// and the graph heal is DEFERRED until after adoption (it is `active_repo_id`-scoped and
    /// would otherwise heal the pre-adoption sole-repo pick).
    AdoptionPending,
}

/// Why a config-bearing open is adopting its repo — decides whether the checkout's working-tree
/// root is RECORDED in `repo_roots` (#427). A recorded root is the "this checkout was INDEXED here"
/// signal `is_root_already_indexed` / `same_identity_join_note` key on, so only an indexing pass
/// may create one. A read-only open registers identity (needed to scope its reads) but must NOT
/// record the root: otherwise a `doctor` / MCP read of a fresh same-identity clone would make it
/// look indexed and let a later empty index prune the shared scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AdoptIntent {
    /// A rebuild / incremental / discover pass that indexes content — records the root.
    Indexing,
    /// A read-only `open_config` (doctor / MCP / query) — registers identity, records NO root.
    ReadOnly,
}

/// A DB-level, repo-agnostic snapshot of a store, produced by
/// [`IndexDatabase::global_store_overview`] for the config-less `doctor` path. `schema` is `None`
/// only when the store file does not exist. Serialized straight into the doctor report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GlobalStoreOverview {
    pub database: PathBuf,
    pub exists: bool,
    pub size_bytes: Option<u64>,
    pub schema: Option<schema::SchemaStatus>,
    pub repos: Vec<schema::RegisteredRepo>,
}

impl IndexDatabase {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_bare(path, BareOpenMode::ConfigLess)
    }

    pub fn database_path(&self) -> &Path {
        self.storage.database_path()
    }

    /// Open the DB at `path` and bring its schema current, migrating FORWARD under the index write
    /// lock when it lags this binary. Shared by every open path; resolves NO repo scope (the caller
    /// does — a bare open via [`sole_repo_id`](schema::sole_repo_id), a config-bearing open via
    /// `register_repo`).
    fn open_and_migrate(path: &Path) -> anyhow::Result<IndexConnection> {
        let storage = IndexConnection::open(path)?;
        // Forward schema migrations are automatic on open (the index is our own derived data — a
        // binary upgrade must not require a manual `migrate`). Only Newer/Dirty/Missing refuse.
        //
        // An `Older` schema is migrated UNDER THE INDEX WRITE LOCK: auto-migration now fires from
        // ordinary read/MCP opens, so a hot-restarted server and a concurrent `query` could both
        // observe `Older` and race `add_column_if_missing`'s check-then-ALTER into a
        // duplicate-column DDL failure. Serializing on the write lock makes one opener migrate
        // while the other waits, then re-checks under the lock (the waiter sees
        // `Compatible` and does nothing). `WriteLock` (not the raw `FileLock`) is REENTRANT
        // on the holding thread: a CLI write command / watcher pass already holds it and
        // opens UNDER it, so a raw re-acquire here would self-deadlock (same process,
        // second fd → flock blocks → 30s timeout, schema never migrates — #226). Reentrant
        // acquire returns immediately when this thread already holds it; a non-holder
        // (read/MCP/init open) still takes the real lock. Compatible/Newer/Dirty/Missing
        // need no lock — `ensure_compatible_or_migrate` returns or refuses without writing.
        if schema::status(storage.connection())?.state == schema::SchemaState::Older {
            // #585: refuse a dev/test build's silent forward-migration of the shared global store
            // BEFORE taking the schema lock — on a box with one global DB the newest-migration
            // binary otherwise wins the schema race for every process. Installed binaries and
            // RAG_RAT_ALLOW_MIGRATE proceed; per-repo/temp DBs are never gated.
            super::migration_gate::MigrationGate::from_env()
                .ensure_migration_permitted(path, schema::SchemaState::Older)?;
            // A6: the GLOBAL schema lock, not a per-repo write lock — a migration rewrites the
            // shared ladder (every repo's tables), so it serializes across all repos.
            // Reentrant on the holding thread, so a CLI write command that opens under
            // its per-repo write lock and migrates here takes a DIFFERENT (schema) lock
            // without self-deadlocking.
            let _lock = rag_rat_base::locks::WriteLock::acquire_schema_timeout(
                path,
                SCHEMA_MIGRATE_LOCK_TIMEOUT,
            )?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "timed out waiting for the schema-migration lock to auto-migrate the schema"
                )
            })?;
            schema::ensure_compatible_or_migrate(
                storage.connection(),
                &crate::index::migration_hooks(),
            )?;
        } else {
            schema::ensure_compatible_or_migrate(
                storage.connection(),
                &crate::index::migration_hooks(),
            )?;
        }
        // #688: the `/3` content projection's two-part projector-version discipline. Its
        // per-stream reproject only ever MAINTAINS an already-current store-global stamp, so a
        // store folded by an older `/3` projector is rebuilt HERE — the open/migrate seam, after
        // migrations, so a store that just gained the V070 tables has them — BEFORE any
        // per-stream write. A no-op when the stamp is current or the store is pre-V070 (the
        // V070-tables guard inside skips cleanly); idempotent and serialized by its own
        // IMMEDIATE txn, so racing openers converge.
        rag_rat_oplog::rebuild_all_content_projections_if_stale(storage.connection())?;
        Ok(storage)
    }

    pub(super) fn open_bare(path: &Path, mode: BareOpenMode) -> anyhow::Result<Self> {
        let mut storage = Self::open_and_migrate(path)?;
        // A bare `open` has no config identity to register, so it scopes to the SOLE repo of a
        // single-repo DB. On a CONSOLIDATED multi-repo DB (A7) there is no sole repo to pick —
        // `sole_repo_id`'s deterministic lexicographic tiebreak would silently serve whichever repo
        // sorts first, the exact ambient-scope assumption phase A exists to kill — so a
        // [`ConfigLess`](BareOpenMode::ConfigLess) open FAILS FAST with the config-bearing remedy
        // instead of degrading. Gated BEFORE the model-manifest heal, which itself resolves
        // `active_repo_id` (⇒ the same first-sorting pick) and could otherwise seed per-repo meta
        // under the wrong repo. An [`AdoptionPending`](BareOpenMode::AdoptionPending) open is
        // exempt: the caller adopts the config's repo immediately after, which re-scopes the
        // connection before any repo-scoped heal runs.
        if matches!(mode, BareOpenMode::ConfigLess)
            && schema::multiple_real_repos(storage.connection())?
        {
            anyhow::bail!(
                "this database holds multiple repos; a bare open cannot choose one. Open through \
                 a rag-rat.toml config (run from the repo's checkout, or pass --config) so the \
                 repo scope is explicit."
            );
        }
        // The model-manifest heal resolves `active_repo_id` — context-less, the sole-repo pick —
        // and on a heal-owed pass its `remove_legacy_models` DELETES that repo's `repo_meta`
        // active-model keys. Safe here only for ConfigLess (the fail-fast above guarantees a
        // single repo, so the pick IS the repo); an AdoptionPending open on a multi-repo DB would
        // mutate the FIRST-SORTING repo's meta while the caller holds only its own repo's lock —
        // so the heal is DEFERRED to the caller, after adopt + set_context scope the connection
        // (the exact ordering `open_config` uses).
        if matches!(mode, BareOpenMode::ConfigLess) {
            ai::ensure_model_manifest(storage.connection())?;
        }
        let repo_id = schema::sole_repo_id(storage.connection())?;
        if let Some(root) = repo_meta(storage.connection(), &repo_id, "source_root")? {
            storage.set_source_root(PathBuf::from(root));
        }
        // Stamp the sole repo's LIVE generation (a heal write on this connection lands on the
        // live generation, not 0) AND install the repo+generation `files` view (A6, P2 review):
        // `set_context` is never called on a bare open, so without a view every unqualified
        // `files` join (lexical search, status counts, the graph heal) resolved to raw
        // `main.files` — which post-A6 also holds STAGED (pre-flip) and SUPERSEDED (pre-gc)
        // generations. This is the round-6 bare-open class on the generation axis; the view
        // closes it for every reader on this connection at once, while deliberately keeping the
        // bare open's CROSS-SCOPE semantics (all commits/worktrees of the sole repo — #360
        // pins that `open` counts more than the base-scoped `open_config`).
        let active_generation = schema::live_files_generation(storage.connection(), &repo_id)?;
        write_repo_generation_view(storage.connection(), &repo_id, active_generation)?;
        let db = Self {
            storage,
            active_repo_id: repo_id,
            active_commit_sha: String::new(),
            active_worktree_id: String::new(),
            active_generation,
            papertrail: papertrail::PapertrailContext::default(),
            config: None,
            _identity_lock: None,
            drift_snapshot: std::sync::Mutex::new(None),
            edge_rewrite_capture: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            logical_symbol_rebuilds: std::sync::atomic::AtomicUsize::new(0),
        };
        if matches!(mode, BareOpenMode::ConfigLess) {
            db.ensure_graph_index_current()?;
            db.ensure_generated_flags_current()?;
        }
        Ok(db)
    }

    pub fn open_config(config: &Config) -> anyhow::Result<Self> {
        let storage = Self::open_and_migrate(&config.database)?;
        let mut db = Self {
            storage,
            active_repo_id: String::new(),
            active_commit_sha: String::new(),
            active_worktree_id: String::new(),
            active_generation: 0,
            // Real usage: resolve the tracker context (config bindings, or auto-detect from the
            // git remote) here, at the boundary. rebuild/open (used by tests and the bare index
            // command) leave it offline.
            papertrail: papertrail::PapertrailContext::resolve(config),
            config: Some(config.clone()),
            _identity_lock: None,
            drift_snapshot: std::sync::Mutex::new(None),
            edge_rewrite_capture: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            logical_symbol_rebuilds: std::sync::atomic::AtomicUsize::new(0),
        };
        db.storage.set_source_root(config.root.clone());
        // Register/adopt BEFORE anything repo-scoped runs, then install the scope context so the
        // model-manifest heal + seed below resolve the correct repo (in a consolidated DB the sole-
        // repo fallback would be ambiguous — the ordering, not the fallback, is load-bearing
        // there). READ-ONLY intent: `open_config` backs doctor / MCP / query reads, so it registers
        // identity but records NO working-tree root (#427) — a read must not mark this checkout
        // "indexed here".
        db.adopt_repo_from_config(config, AdoptIntent::ReadOnly)?;
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        db.set_context(&commit_sha, &worktree_id)?;
        ai::ensure_model_manifest(db.storage.connection())?;
        db.ensure_graph_index_current()?;
        db.ensure_generated_flags_current()?;
        // Adopt the configured embedding model as the index's active model when it has none yet, so
        // reconcile targets it (and its "install" hint names it) instead of the hash fallback
        // (#394).
        ai::seed_active_embedding_model(
            db.storage.connection(),
            config.llm.embedding.backend.model_id(),
        )?;
        Ok(db)
    }

    /// Resolve this config's repo IDENTITY and register/adopt it, stamping `self.active_repo_id`.
    /// Identity-resolution failures split by class
    /// ([`RepoIdentityError`](rag_rat_base::repo_identity::RepoIdentityError)):
    /// - `Absent` (not a git repo / unborn HEAD — many tests, and any bare temp-dir index) is NOT
    ///   an error: fall back to the sole registered repo (the placeholder on a fresh DB), leaving
    ///   the DB single-repo and un-adopted exactly as before A3.
    /// - `Rejected` (a pinned reserved `[index] repo_id`, or an unreadable / root-less history)
    ///   PROPAGATES: falling back would silently scope the DB to the placeholder and bury the
    ///   configuration problem the error names, leaving rows unadopted under the legacy id. A cut
    ///   shallow clone is NOT rejected — it resolves to a `LocalOnly` id and registers normally.
    ///
    /// A genuine registration refusal (a different repo already owns a consolidated DB) also
    /// propagates, via `register_repo` itself.
    pub(super) fn adopt_repo_from_config(
        &mut self,
        config: &Config,
        intent: AdoptIntent,
    ) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        let repo_id = match rag_rat_base::repo_identity::resolve_repo_identity(
            &config.root,
            config.repo_id_override.as_deref(),
        ) {
            // #427: an INDEXING adoption records this checkout's working-tree root in `repo_roots`
            // (the "this checkout was indexed here" signal); a READ-ONLY adoption (`open_config`
            // from doctor / MCP / query) registers identity but does NOT record the root, so a mere
            // read can't make an unindexed checkout look indexed (which would let a later empty
            // index prune the shared scope).
            Ok(identity) => {
                let now = rag_rat_base::time::now_ms();
                match intent {
                    AdoptIntent::Indexing => schema::register_repo(
                        conn,
                        &identity,
                        &config.root,
                        now,
                        &crate::index::migration_hooks(),
                    )?,
                    AdoptIntent::ReadOnly => schema::register_repo_read_only(
                        conn,
                        &identity,
                        &config.root,
                        now,
                        &crate::index::migration_hooks(),
                    )?,
                }
            },
            Err(err) if err.is_absent() => {
                // STRUCTURAL BACKSTOP (Codex batch 8, finding 5): an identity-less root may
                // sole-pick only on a SINGLE-repo database. On a multi-repo store (an explicit
                // `database` pin at the global file or any shared path), `sole_repo_id`'s
                // lexicographic tiebreak would silently adopt a SIBLING repo's scope and write
                // this project's rows under it. Refuse with the remedy instead — the same
                // doctrine as the config-less bare-open fail-fast and the healers' witness: no
                // entrance may guess a repo on a multi-repo database. (`Config::load` already
                // refuses the global-pin shape at resolution; this closes every other entrance.)
                if schema::multiple_real_repos(conn)? {
                    anyhow::bail!(
                        "this root has no resolvable repo identity (not a committed git repo), \
                         and the configured database holds multiple repos — refusing to guess \
                         which one this project is. Add `[index] repo_id = \"...\"` to pin an \
                         identity, or point `database` at a per-repo file"
                    );
                }
                schema::sole_repo_id(conn)?
            },
            Err(err) => return Err(err.into()),
        };
        // FENCE-GAP CLOSE (A6, batch-5 P2): a LOCK-DISCIPLINED writer's held lock must match the
        // repo id it writes under. The entry lock was keyed by the id DERIVED before open; if the
        // clone was unshallowed in between, the id just resolved (and upgraded to) is a DIFFERENT
        // discriminator — the rest of this command would write the portable repo's rows under
        // only the stale `local:` lock, concurrent with any fresh portable-lock writer. Detect
        // "I hold SOME per-repo lock for this DB, but not the resolved id's" and acquire the
        // resolved id's lock for this connection's lifetime. Lockless openers (MCP reads/heals)
        // hold nothing and stay lockless. This is the out-of-order edge of the canonical lock
        // order (the resolved portable id sorts before the held `local:` one), so it is BOUNDED —
        // a timeout is a retryable error, never a hang (see the locks module doc).
        let db_path = self.storage.database_path().to_path_buf();
        if rag_rat_base::locks::thread_holds_any_repo_write_lock(&db_path)
            && !rag_rat_base::locks::thread_holds_write_lock(&db_path, &repo_id)
        {
            self._identity_lock = Some(
                rag_rat_base::locks::WriteLock::acquire_timeout(
                    &db_path,
                    &repo_id,
                    SCHEMA_MIGRATE_LOCK_TIMEOUT,
                )?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "the repo identity changed to {repo_id} underneath this command (its \
                         write lock is keyed by the old identity) and the new identity's lock is \
                         still held elsewhere; re-run the command"
                    )
                })?,
            );
        }
        self.active_repo_id = repo_id;
        Ok(())
    }

    /// Open the index **read-only** for a pure-read tool, returning `None` when the index still
    /// owes a write to be brought current. A `SQLITE_OPEN_READ_ONLY` connection can never acquire
    /// the main write lock, so a read served through it is structurally immune to being locked out
    /// by a concurrent writer (the watcher, a heal, another client) — the fix for the intermittent
    /// "database is locked" under concurrent MCP clients (#143). Unlike `open_config` it performs
    /// NO on-open heal writes (`ensure_model_manifest` / `ensure_graph_index_current`).
    ///
    /// Returns `Ok(None)` — caller falls back to the read-write `open_config`, which heals once and
    /// after which reads are lock-free again — when any of these is true:
    /// - the DB has never been opened for write (read-only open errors),
    /// - the schema is not `Compatible` (a forward migrate is owed; that is a write),
    /// - the graph index is stale (`ensure_graph_index_current` would rebuild — a write),
    /// - the model manifest is not yet current (`ensure_model_manifest` would write).
    ///
    /// The temp-scope view (`set_context` → `install_scope_view`) is still installed: it writes
    /// only the per-connection `temp.*` database, which is writable even on a read-only main DB.
    pub fn try_open_config_read_only(config: &Config) -> anyhow::Result<Option<Self>> {
        let mut storage = match IndexConnection::open_read_only_blocking(&config.database) {
            Ok(storage) => storage,
            // Never opened for write yet (no file / no WAL) — let the read-write path create it.
            Err(_) => return Ok(None),
        };
        if schema::status(storage.connection())?.state != schema::SchemaState::Compatible {
            return Ok(None);
        }
        // Resolve the repo this CONFIG maps to on the read-only connection, WITHOUT registering (a
        // write). A consolidated DB (post-A7) can hold several repos; the config-blind sole-repo
        // pick would bind a SIBLING, so `resolve_config_repo_id` scopes by the config's identity /
        // recorded root instead. `None` — the repo is not provably registered here (a fresh DB, or
        // a consolidated one whose config root is not recorded) — means a write is owed to
        // register it, so fall back to the read-write `open_config` (same posture as the
        // gates below).
        let Some(repo_id) = schema::resolve_config_repo_id(
            storage.connection(),
            &config.root,
            config.repo_id_override.as_deref(),
        )?
        else {
            return Ok(None);
        };
        storage.set_source_root(config.root.clone());
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        let mut db = Self {
            storage,
            active_repo_id: repo_id,
            active_commit_sha: String::new(),
            active_worktree_id: String::new(),
            active_generation: 0,
            papertrail: papertrail::PapertrailContext::resolve(config),
            config: Some(config.clone()),
            _identity_lock: None,
            drift_snapshot: std::sync::Mutex::new(None),
            edge_rewrite_capture: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            logical_symbol_rebuilds: std::sync::atomic::AtomicUsize::new(0),
        };
        // Install the scope context BEFORE the heal-owed gates: `set_context` mirrors the resolved
        // `repo_id` into `temp.connection_context` (writable even on a read-only main DB), so the
        // gates below that resolve `active_repo_id` from the connection (the model manifest + the
        // embedding-model seed) see the CONFIG's repo — not the config-blind sole repo, which is a
        // SIBLING in a consolidated DB and would make them falsely report a heal is owed under an
        // empty scope. The graph / generated-flags gates read `repo_id` explicitly.
        db.set_context(&commit_sha, &worktree_id)?;
        let conn = db.storage.connection();
        if repo_meta(conn, &db.active_repo_id, "graph_index_version")?.as_deref()
            != Some(GRAPH_INDEX_VERSION)
        {
            return Ok(None);
        }
        // A stale generated-flags version owes a re-derive (a write); fall back to read-write so it
        // heals once, after which reads are lock-free again (#202, same posture as the graph gate).
        if read_meta(conn, GENERATED_FLAGS_VERSION_KEY)?.as_deref() != Some(GENERATED_FLAGS_VERSION)
        {
            return Ok(None);
        }
        if !ai::model_manifest_is_current(conn)? {
            return Ok(None);
        }
        // A fresh index owes an active-embedding-model seed from config (a write); fall back to the
        // read-write open so it heals once (#394, same posture as the manifest / graph gates).
        if ai::active_embedding_model_seed_owed(conn, config.llm.embedding.backend.model_id())? {
            return Ok(None);
        }
        Ok(Some(db))
    }

    /// Set the GitHub repo context explicitly for tests and embedding callers.
    pub fn set_papertrail_context(&mut self, default_repo: Option<&str>) {
        self.papertrail = papertrail::PapertrailContext::new(default_repo);
    }

    pub fn migrate(path: &Path) -> anyhow::Result<schema::SchemaStatus> {
        Self::migrate_with_fastembed_cache(path, None)
    }

    /// Bring the schema at `path` current and NOTHING ELSE — no model-manifest heal, no fastembed
    /// cache recovery. For callers whose operation's SUBJECT REPO IS NOT REGISTERED YET
    /// (`rag-rat consolidate`, future importers): the open-time healers attribute their per-repo
    /// `repo_meta` reads/writes via the scoped-repo witness, and the witness's single-repo arm
    /// picks the sole REGISTERED repo — which for an unregistered subject is a SIBLING, healed
    /// under the wrong repo's locks. Such callers must use this schema-only path; any owed sibling
    /// heal belongs to that sibling's own next scoped open.
    pub fn migrate_schema_only(path: &Path) -> anyhow::Result<schema::SchemaStatus> {
        let storage = IndexConnection::open(path)?;
        let status = schema::status(storage.connection())?;
        match status.state {
            schema::SchemaState::Newer | schema::SchemaState::Dirty => {
                anyhow::bail!("{}", status.message);
            },
            schema::SchemaState::Compatible => {},
            schema::SchemaState::Missing | schema::SchemaState::Older => {
                // Serializes on the GLOBAL schema lock and re-checks under it, like every
                // schema-apply path (see `apply_schema_under_lock`).
                Self::apply_schema_under_lock(path, storage.connection())?;
            },
        }
        schema::status(storage.connection())
    }

    pub(super) fn migrate_with_fastembed_cache(
        path: &Path,
        fastembed_cache_dir: Option<&Path>,
    ) -> anyhow::Result<schema::SchemaStatus> {
        let storage = IndexConnection::open(path)?;
        let status = schema::status(storage.connection())?;
        match status.state {
            schema::SchemaState::Newer | schema::SchemaState::Dirty => {
                anyhow::bail!("{}", status.message);
            },
            schema::SchemaState::Compatible => {},
            schema::SchemaState::Missing | schema::SchemaState::Older => {
                // Every schema-APPLY path serializes on the GLOBAL schema lock and RE-CHECKS
                // under it (the racer may have finished) — see `apply_schema_under_lock`.
                Self::apply_schema_under_lock(path, storage.connection())?;
            },
        }
        ai::ensure_model_manifest(storage.connection())?;
        if let Some(fastembed_cache_dir) = fastembed_cache_dir {
            ai::recover_cached_fastembed_model_from(storage.connection(), fastembed_cache_dir)?;
        } else {
            ai::recover_cached_fastembed_model(storage.connection())?;
        }
        schema::status(storage.connection())
    }

    pub fn migration_check(path: &Path) -> anyhow::Result<schema::SchemaStatus> {
        let storage = IndexConnection::open(path)?;
        schema::status(storage.connection())
    }

    /// A DB-LEVEL, repo-agnostic overview of the store at `path` — for `doctor` invoked OUTSIDE any
    /// rag-rat repo, where the repo-scoped opens (`open`, `open_config`) deliberately refuse to
    /// guess a repo in a consolidated multi-repo store. Reports on-disk presence/size, the schema
    /// status, and the registry of real repos the store holds (each with its recorded roots).
    /// Side-effect-free when the file is ABSENT (returns `exists: false` without opening/creating
    /// it); the registry is read only when the schema is `Compatible` (a Missing/Newer/Dirty schema
    /// has no queryable `repos` table). The registry read is READ-ONLY.
    pub fn global_store_overview(path: &Path) -> anyhow::Result<GlobalStoreOverview> {
        if !path.is_file() {
            return Ok(GlobalStoreOverview {
                database: path.to_path_buf(),
                exists: false,
                size_bytes: None,
                schema: None,
                repos: Vec::new(),
            });
        }
        let size_bytes = std::fs::metadata(path).ok().map(|meta| meta.len());
        // READ-ONLY throughout: this diagnostic must work on a read-only mount / backup, so it must
        // not use the read-WRITE `migration_check` (whose `IndexConnection::open` runs
        // write-oriented setup pragmas). One read-only connection serves both the schema
        // status and the registry read. NB: a read-only open never auto-migrates, which is
        // correct here — the report states the schema STATE, it does not change it.
        let storage = IndexConnection::open_read_only_blocking(path)?;
        let schema = schema::status(storage.connection())?;
        let repos = if schema.state == schema::SchemaState::Compatible {
            schema::registered_repos(storage.connection())?
        } else {
            Vec::new()
        };
        Ok(GlobalStoreOverview {
            database: path.to_path_buf(),
            exists: true,
            size_bytes,
            schema: Some(schema),
            repos,
        })
    }

    /// Create-or-migrate the schema for a full [`rebuild`](Self::rebuild). Leaves the repo scope
    /// UNSET (`active_repo_id` empty): `rebuild` registers/adopts the repo and installs the scope
    /// context itself (so the model-manifest heal and every direct-scoped write see the right repo,
    /// even on a consolidated DB), then stamps `source_root` from the config. The manifest heal and
    /// source-root restore therefore move to `rebuild`, out of this low-level helper.
    pub(super) fn create_or_migrate(path: &Path) -> anyhow::Result<Self> {
        let storage = IndexConnection::open(path)?;
        // Double-checked schema apply (batch-4 P2): two repos' concurrent `index --full` against
        // one shared Missing/Older DB reach this constructor simultaneously, and an un-serialized
        // `schema::apply` races itself (`add_column_if_missing`'s check-then-ALTER trips duplicate
        // -column errors; the dirty-marker dance churns). Skip when already Compatible — `apply`
        // on a Compatible DB is a proven data no-op (the full-ladder replay tests), so skipping is
        // both safe and cheaper — else serialize on the GLOBAL schema lock and re-check under it.
        if schema::status(storage.connection())?.state != schema::SchemaState::Compatible {
            Self::apply_schema_under_lock(path, storage.connection())?;
        }
        Ok(Self {
            storage,
            active_repo_id: String::new(),
            active_commit_sha: String::new(),
            active_worktree_id: String::new(),
            active_generation: 0,
            papertrail: papertrail::PapertrailContext::default(),
            config: None,
            _identity_lock: None,
            drift_snapshot: std::sync::Mutex::new(None),
            edge_rewrite_capture: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            logical_symbol_rebuilds: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Run `schema::apply` under the GLOBAL schema-migration lock, RE-CHECKING the state once the
    /// lock is held (double-checked: the concurrent applier we serialized behind may have finished
    /// the work, in which case this is a no-op). EVERY path that can APPLY schema goes through
    /// here or `open_and_migrate`'s equivalent (batch-4 P2 rule) — the schema ladder is shared by
    /// all repos in the DB file, so appliers must serialize across repos, which the per-repo write
    /// flock deliberately does not do.
    fn apply_schema_under_lock(path: &Path, conn: &rusqlite::Connection) -> anyhow::Result<()> {
        let _lock = rag_rat_base::locks::WriteLock::acquire_schema_timeout(
            path,
            SCHEMA_MIGRATE_LOCK_TIMEOUT,
        )?
        .ok_or_else(|| {
            anyhow::anyhow!("timed out waiting for the schema-migration lock to apply the schema")
        })?;
        let state = schema::status(conn)?.state;
        if state != schema::SchemaState::Compatible {
            // #585: a dev/test build must not silently forward-migrate the shared global store —
            // it would strand every process still on an older binary. Refused here (Older only;
            // Missing is first-time init) unless RAG_RAT_ALLOW_MIGRATE / an installed binary.
            super::migration_gate::MigrationGate::from_env()
                .ensure_migration_permitted(path, state)?;
            schema::apply(conn, &crate::index::migration_hooks())?;
        }
        Ok(())
    }

    pub fn set_context(&mut self, commit_sha: &str, worktree_id: &str) -> anyhow::Result<()> {
        // A reader / incremental open scopes to the repo's LIVE generation (A6) — the pointer a
        // full rebuild flips once its staged generation is complete. The rebuild connection
        // overrides this with its WRITE generation via `set_context_at_generation`.
        let generation =
            schema::live_files_generation(self.storage.connection(), &self.active_repo_id)?;
        self.set_context_at_generation(commit_sha, worktree_id, generation)
    }

    /// Install the scope VIEW for an arbitrary `(commit, worktree)` at an explicit generation
    /// WITHOUT touching this connection's `active_*` fields — the rebuild tail's overlay
    /// re-resolution seam (A6). The terminal flip transaction swaps the view to each carried
    /// overlay scope (so `resolve_overlay_edges` resolves that overlay's edges against its view
    /// over the freshly staged base), then back to the base scope; the connection's own identity
    /// (`active_commit_sha` / `active_worktree_id` / `active_generation`) must stay the base
    /// rebuild's throughout, so the writer-side stamps and `active_scope_is_linked_overlay`
    /// gates are unaffected by the temporary view swaps.
    pub(super) fn install_view_for_scope(
        &self,
        commit_sha: &str,
        worktree_id: &str,
        generation: i64,
    ) -> rusqlite::Result<()> {
        write_scope_view(self.storage.connection(), &ScopeContext {
            repo_id: self.active_repo_id.clone(),
            commit_sha: commit_sha.to_string(),
            worktree_id: worktree_id.to_string(),
            generation,
        })
    }

    /// [`set_context`] pinned to an EXPLICIT `files.generation` — the full rebuild's seam (A6). The
    /// rebuild installs the scope view (and stamps every direct-scoped write, via
    /// `self.active_generation`) at the WRITE generation N+1 it is building, so its own
    /// edge-resolution / logical-symbol reads see only the generation being built; concurrent
    /// readers stay on the live generation N until the flip.
    pub(crate) fn set_context_at_generation(
        &mut self,
        commit_sha: &str,
        worktree_id: &str,
        generation: i64,
    ) -> anyhow::Result<()> {
        self.active_commit_sha = commit_sha.to_string();
        self.active_worktree_id = worktree_id.to_string();
        self.active_generation = generation;
        // The repo dimension comes from `self.active_repo_id` (resolved at open), not re-derived
        // from the connection — a consolidated DB's sole-repo fallback would be ambiguous. This is
        // the one caller that scopes to a SPECIFIC repo without reading it back from the context.
        write_scope_view(self.storage.connection(), &ScopeContext {
            repo_id: self.active_repo_id.clone(),
            commit_sha: commit_sha.to_string(),
            worktree_id: worktree_id.to_string(),
            generation,
        })?;
        Ok(())
    }

    /// Whether this connection is scoped to a LINKED-worktree overlay (a non-empty
    /// `active_worktree_id` that differs from the base checkout's own id, derived from
    /// `source_root`). The lazy heal paths (`heal_file`, `heal_index`) read file bytes from
    /// `source_root` (the MAIN checkout), so writing under a linked overlay scope would shadow the
    /// branch's rows with MAIN's content; they SKIP the write in that case and leave the overlay to
    /// `index_worktree_overlay`, the one writer allowed to maintain it (#219 review).
    pub(crate) fn active_scope_is_linked_overlay(&self) -> bool {
        if self.active_worktree_id.is_empty() {
            return false;
        }
        match self.storage.source_root() {
            Some(root) => self.active_worktree_id != worktree_id_of(root),
            None => false,
        }
    }

    /// Re-scope this connection to a caller's `worktree` (a linked-worktree checkout), serving its
    /// overlay over the base. The base commit stays `root`'s indexed HEAD; only the `worktree_id`
    /// selects the overlay. A `None`, main, foreign, or unreadable `worktree` resolves to `root`'s
    /// own scope — never the wrong repo (the validation lives in `resolve_worktree_scope`). The
    /// query open path calls this after opening so a `worktree`-scoped request serves the
    /// overlay (#219 stage 3); the overlay rows themselves are maintained by
    /// `index_worktree_overlay` (#219 stages 2/5). `root` is passed explicitly (not read from
    /// `self.config`) so it works on every open.
    pub fn use_worktree_scope(
        &mut self,
        root: &Path,
        worktree: Option<&Path>,
    ) -> anyhow::Result<()> {
        let (commit_sha, worktree_id) = resolve_worktree_scope(root, worktree);
        self.set_context(&commit_sha, &worktree_id)
    }
}

/// Install the per-connection scope view for a worktree-aware query on a RAW connection — the
/// Claude Code hooks (SessionStart orientation, PreToolUse grep-augmentation) and the MCP hook
/// listener open an `IndexConnection` directly, not `IndexDatabase`, so they can't use
/// `use_worktree_scope`. Resolves the OVERLAY scope from `config_root` (the main worktree, where
/// the base index lives) and `cwd` (the session's working dir): when `cwd` is a linked worktree of
/// `config_root`'s repo, the view serves that branch's overlay on the base; otherwise (main /
/// foreign / unreadable) it is the base scope.
///
/// The REPO dimension is passed EXPLICITLY (`repo_id`) rather than re-resolved from the raw
/// connection: a raw conn has no scope context installed, so the free-conn
/// [`schema::active_repo_id`] fallback would pick the config-blind sole repo — a SIBLING in a
/// consolidated DB. The caller resolves it from its config (where the identity / override is
/// reachable) via [`resolve_scope_repo_id`]; an empty string binds a scope that matches nothing —
/// the safe result when the config's repo can't be proven, never a sibling's rows. `pub` so the CLI
/// hook + the MCP listener (other crates) can scope their context to the worktree the session is
/// actually in (#219).
pub fn install_worktree_scope_view(
    conn: &rusqlite::Connection,
    repo_id: &str,
    config_root: &Path,
    cwd: &Path,
) -> anyhow::Result<()> {
    let (commit_sha, worktree_id) = resolve_worktree_scope(config_root, Some(cwd));
    // A6: this raw-conn READ installer scopes to the repo's LIVE generation from `repo_meta` (an
    // empty `repo_id` — the sibling-safe "match nothing" case — reads generation 0, still matching
    // nothing, since no row carries the empty repo).
    let generation = schema::live_files_generation(conn, repo_id)?;
    write_scope_view(conn, &ScopeContext {
        repo_id: repo_id.to_string(),
        commit_sha,
        worktree_id,
        generation,
    })?;
    Ok(())
}

/// Resolve the `repo_id` a config maps to on a READ-ONLY connection, without registering — the
/// public entry point for the raw-connection scope-view callers (the Claude Code / MCP hooks) that
/// live in other crates. Thin wrapper over [`schema::resolve_config_repo_id`]; see it for the
/// resolution routes and the `None` semantics. Pass the resolved id into
/// [`install_worktree_scope_view`] (or `unwrap_or_default()` for an empty — sibling-safe — scope
/// when the repo can't be proven).
pub fn resolve_scope_repo_id(
    conn: &rusqlite::Connection,
    config_root: &Path,
    repo_id_override: Option<&str>,
) -> anyhow::Result<Option<String>> {
    Ok(schema::resolve_config_repo_id(conn, config_root, repo_id_override)?)
}

/// The active scope a connection's `files` view is filtered to: the repo dimension (A3) plus the
/// existing commit/worktree dimensions. Threaded into [`write_scope_view`], which mirrors it into
/// `temp.connection_context` so free-conn helpers (`schema::active_repo_id`, `edges::resolve`)
/// recover the same values.
pub(crate) struct ScopeContext {
    pub repo_id: String,
    pub commit_sha: String,
    pub worktree_id: String,
    /// The `files.generation` the view filters on (A6): the repo's LIVE generation for a
    /// reader/incremental open, the WRITE generation N+1 for the connection driving a full
    /// rebuild.
    pub generation: i64,
}

/// Install the scope view on a RAW connection that has no `IndexDatabase` to carry
/// `active_repo_id`, resolving the repo dimension from the connection via
/// [`schema::active_repo_id`] (the sole repo of a single-repo DB, or the placeholder on an
/// un-adopted test DB — matching the placeholder-defaulted `files` rows those tests insert).
/// TEST-ONLY now: the raw-conn hook listeners resolve the repo id explicitly from their config and
/// pass it into [`install_worktree_scope_view`], so only the graph tests (which seed rows under the
/// sole/placeholder repo) reach this config-blind resolver. `IndexDatabase::set_context` likewise
/// passes an explicit repo id (the multi-repo path). Not `#[cfg(test)]`: the read-layer crate's
/// tests consume it through the dev-dependency, and the gate does not propagate cross-crate.
pub fn install_scope_view(
    conn: &rusqlite::Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> rusqlite::Result<()> {
    let repo_id = schema::active_repo_id(conn)?;
    let generation = schema::active_generation(conn)?;
    write_scope_view(conn, &ScopeContext {
        repo_id,
        commit_sha: commit_sha.to_string(),
        worktree_id: worktree_id.to_string(),
        generation,
    })
}

/// Installs the per-connection repo/commit/worktree scoping view; callers query `files` afterward
/// and see only the active context. The `files` view filters on `repo_id` FIRST (A3) so a
/// consolidated DB never leaks another repo's rows through the view — every read path that goes
/// through `temp.files` is repo-scoped for free.
fn write_scope_view(conn: &rusqlite::Connection, ctx: &ScopeContext) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
            CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);
        ",
    )?;

    let mut stmt =
        conn.prepare("INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES (?1, ?2)")?;
    stmt.execute(params![schema::CONNECTION_CONTEXT_REPO_KEY, ctx.repo_id])?;
    stmt.execute(params!["commit_sha", ctx.commit_sha])?;
    stmt.execute(params!["worktree_id", ctx.worktree_id])?;
    // A6: the file generation the view filters on. Stored as TEXT beside the other context keys;
    // the INTEGER `generation` column's numeric affinity coerces it back in the comparisons
    // below.
    stmt.execute(params![schema::CONNECTION_CONTEXT_GENERATION_KEY, ctx.generation.to_string()])?;
    drop(stmt);

    // Every branch (and the shadowing sub-select) is repo- AND generation-scoped (A3 + A6):
    // `worktree_id`/`commit_sha` alone are not globally unique across repos (forks share a commit;
    // the empty base scope is shared), so the `repo_id` predicate keeps a sibling repo's rows out;
    // the `generation` predicate keeps a superseded full-rebuild generation (dead until gc sweeps
    // it) out, so a reader sees the COMPLETE old generation until the rebuild flips
    // `live_files_generation`, then the complete new one — never a half-built mix.
    conn.execute_batch(
        "
            DROP VIEW IF EXISTS temp.files;
            CREATE TEMP VIEW temp.files AS
            SELECT id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, \
         indexed_revision, commit_sha, worktree_id, has_test_code
            FROM main.files
            WHERE repo_id = (SELECT value FROM temp.connection_context WHERE key = 'repo_id')
              AND generation = (SELECT value FROM temp.connection_context WHERE key = \
         'files_generation')
              AND worktree_id = (SELECT value FROM temp.connection_context WHERE key = \
         'worktree_id') AND worktree_id != '' AND kind != 'deleted'
            UNION ALL
            SELECT id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, \
         indexed_revision, commit_sha, worktree_id, has_test_code
            FROM main.files
            WHERE repo_id = (SELECT value FROM temp.connection_context WHERE key = 'repo_id')
              AND generation = (SELECT value FROM temp.connection_context WHERE key = \
         'files_generation')
              AND commit_sha = (SELECT value FROM temp.connection_context WHERE key = 'commit_sha')
              AND commit_sha != ''
              AND path NOT IN (
                  SELECT path FROM main.files
                  WHERE repo_id = (SELECT value FROM temp.connection_context WHERE key = 'repo_id')
                    AND generation = (SELECT value FROM temp.connection_context WHERE key = \
         'files_generation')
                    AND worktree_id = (SELECT value FROM temp.connection_context WHERE key = \
         'worktree_id')
                    AND worktree_id != ''
              );
        ",
    )?;

    Ok(())
}

/// Repo + generation-only `files` view for the BARE open (A6, P2 review). A bare
/// `IndexDatabase::open` (the MCP `call_tool(database, …)` read path, tests, `doctor`)
/// deliberately serves ALL commits/worktrees of the sole repo — #360 pins that its counts exceed
/// the base-scoped `open_config` — so it cannot install the commit/worktree-scoped view above.
/// But leaving `files` unqualified resolved it to raw `main.files`, which post-A6 also holds
/// STAGED (pre-flip) and SUPERSEDED (pre-gc) generations: the round-6 bare-open class on the
/// generation axis. This view keeps the cross-scope semantics while pinning `repo_id` and the
/// LIVE generation. The context rows are written too, so free-conn helpers
/// (`schema::active_repo_id` / `schema::active_generation`) resolve the same values;
/// `commit_sha`/`worktree_id` stay empty (no checkout scope).
fn write_repo_generation_view(
    conn: &rusqlite::Connection,
    repo_id: &str,
    generation: i64,
) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
    )?;
    let mut stmt =
        conn.prepare("INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES (?1, ?2)")?;
    stmt.execute(params![schema::CONNECTION_CONTEXT_REPO_KEY, repo_id])?;
    stmt.execute(params!["commit_sha", ""])?;
    stmt.execute(params!["worktree_id", ""])?;
    stmt.execute(params![schema::CONNECTION_CONTEXT_GENERATION_KEY, generation.to_string()])?;
    drop(stmt);
    conn.execute_batch(
        "
            DROP VIEW IF EXISTS temp.files;
            CREATE TEMP VIEW temp.files AS
            SELECT id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, \
         indexed_revision, commit_sha, worktree_id, has_test_code
            FROM main.files
            WHERE repo_id = (SELECT value FROM temp.connection_context WHERE key = 'repo_id')
              AND generation = (SELECT value FROM temp.connection_context WHERE key = \
         'files_generation');
        ",
    )?;
    Ok(())
}

#[cfg(test)]
mod global_store_overview_tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use rag_rat_base::repo_identity::{RepoIdentity, RepoIdentityClass};
    use rag_rat_db::schema::{self, register_repo};
    use rag_rat_db::storage::IndexConnection;

    use crate::index::IndexDatabase;

    static N: AtomicU64 = AtomicU64::new(0);

    fn temp_db() -> PathBuf {
        let id = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("ragrat-gso-{}-{id}", std::process::id()))
            .join("rag-rat.sqlite")
    }

    /// An ABSENT store is reported as `exists: false` WITHOUT opening or creating the file —
    /// `doctor` invoked on a machine that has never indexed anything must not conjure an empty
    /// global store.
    #[test]
    fn overview_reports_an_absent_store_without_creating_it() {
        let path = temp_db();
        let overview = IndexDatabase::global_store_overview(&path).unwrap();
        assert!(!overview.exists);
        assert!(overview.schema.is_none());
        assert!(overview.repos.is_empty());
        assert!(!path.exists(), "reporting an absent store must not create it");
    }

    /// A store with a registered repo lists it (with its recorded root) and EXCLUDES the
    /// `__unassigned__` adoption placeholder — the DB-level report the config-less `doctor` shows.
    #[test]
    fn overview_lists_registered_repos_excluding_the_placeholder() {
        let path = temp_db();
        {
            let conn = IndexConnection::open(&path).unwrap();
            schema::apply(conn.connection(), &crate::index::migration_hooks()).unwrap();
            register_repo(
                conn.connection(),
                &RepoIdentity {
                    repo_id: "repo-xyz".to_string(),
                    display_name: "demo".to_string(),
                    class: RepoIdentityClass::Portable,
                    shallow_boundary: Vec::new(),
                },
                Path::new("/src/demo"),
                42,
                &crate::index::migration_hooks(),
            )
            .unwrap();
        }

        let overview = IndexDatabase::global_store_overview(&path).unwrap();
        assert!(overview.exists);
        assert_eq!(overview.schema.unwrap().state, schema::SchemaState::Compatible);
        assert_eq!(overview.repos.len(), 1, "the '__unassigned__' placeholder is excluded");
        let repo = &overview.repos[0];
        assert_eq!(repo.repo_id, "repo-xyz");
        assert_eq!(repo.display_name, "demo");
        assert_eq!(repo.roots, vec!["/src/demo".to_string()]);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
