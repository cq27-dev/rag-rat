use super::*;

/// How long an `open` waits for the index write lock before giving up on auto-migrating a schema
/// that lags this binary. Generous — a concurrent migrator (or a watcher maintenance pass holding
/// the lock) finishes well within it; a timeout surfaces an explicit error, never a silent
/// half-open.
const SCHEMA_MIGRATE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl IndexDatabase {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_graph_check(path, true)
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
            let _lock =
                crate::locks::WriteLock::acquire_timeout(path, SCHEMA_MIGRATE_LOCK_TIMEOUT)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "timed out waiting for the index write lock to auto-migrate the schema"
                        )
                    })?;
            schema::ensure_compatible_or_migrate(storage.connection())?;
        } else {
            schema::ensure_compatible_or_migrate(storage.connection())?;
        }
        Ok(storage)
    }

    pub(super) fn open_with_graph_check(path: &Path, check_graph: bool) -> anyhow::Result<Self> {
        let mut storage = Self::open_and_migrate(path)?;
        ai::ensure_model_manifest(storage.connection())?;
        // A bare `open` has no config identity to register, so it scopes to the SOLE repo of a
        // single-repo DB (a consolidated multi-repo DB is only reached through `open_config`, which
        // registers). The model-manifest heal above resolves the same sole repo via the no-context
        // fallback in `schema::active_repo_id`.
        let repo_id = schema::sole_repo_id(storage.connection())?;
        if let Some(root) = repo_meta(storage.connection(), &repo_id, "source_root")? {
            storage.set_source_root(PathBuf::from(root));
        }
        let db = Self {
            storage,
            active_repo_id: repo_id,
            active_commit_sha: String::new(),
            active_worktree_id: String::new(),
            github: github::GitHubContext::default(),
            config: None,
        };
        if check_graph {
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
            // Real usage: resolve the GitHub repo context from the local `gh` CLI here, at the
            // boundary. rebuild/open (used by tests and the bare index command) leave it offline.
            github: github::GitHubContext::from_gh(),
            config: Some(config.clone()),
        };
        db.storage.set_source_root(config.root.clone());
        // Register/adopt BEFORE anything repo-scoped runs, then install the scope context so the
        // model-manifest heal + seed below resolve the correct repo (in a consolidated DB the sole-
        // repo fallback would be ambiguous — the ordering, not the fallback, is load-bearing
        // there).
        db.adopt_repo_from_config(config)?;
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
    /// ([`RepoIdentityError`](crate::repo_identity::RepoIdentityError)):
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
    pub(super) fn adopt_repo_from_config(&mut self, config: &Config) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        let repo_id = match crate::repo_identity::resolve_repo_identity(
            &config.root,
            config.repo_id_override.as_deref(),
        ) {
            Ok(identity) => schema::register_repo(conn, &identity, &config.root, schema::now_ms())?,
            Err(err) if err.is_absent() => schema::sole_repo_id(conn)?,
            Err(err) => return Err(err.into()),
        };
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
            github: github::GitHubContext::from_gh(),
            config: Some(config.clone()),
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

    /// Set the GitHub repo context explicitly (tests / non-gh callers), so the library never
    /// shells out to `gh`.
    pub fn set_github_context(&mut self, default_repo: Option<&str>, gh_available: bool) {
        self.github = github::GitHubContext::new(default_repo, gh_available);
    }

    pub fn migrate(path: &Path) -> anyhow::Result<schema::SchemaStatus> {
        Self::migrate_with_fastembed_cache(path, None)
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
                schema::apply(storage.connection())?;
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

    /// Create-or-migrate the schema for a full [`rebuild`](Self::rebuild). Leaves the repo scope
    /// UNSET (`active_repo_id` empty): `rebuild` registers/adopts the repo and installs the scope
    /// context itself (so the model-manifest heal and every direct-scoped write see the right repo,
    /// even on a consolidated DB), then stamps `source_root` from the config. The manifest heal and
    /// source-root restore therefore move to `rebuild`, out of this low-level helper.
    pub(super) fn create_or_migrate(path: &Path) -> anyhow::Result<Self> {
        let storage = IndexConnection::open(path)?;
        schema::apply(storage.connection())?;
        Ok(Self {
            storage,
            active_repo_id: String::new(),
            active_commit_sha: String::new(),
            active_worktree_id: String::new(),
            github: github::GitHubContext::default(),
            config: None,
        })
    }

    pub fn set_context(&mut self, commit_sha: &str, worktree_id: &str) -> anyhow::Result<()> {
        self.active_commit_sha = commit_sha.to_string();
        self.active_worktree_id = worktree_id.to_string();
        // The repo dimension comes from `self.active_repo_id` (resolved at open), not re-derived
        // from the connection — a consolidated DB's sole-repo fallback would be ambiguous. This is
        // the one caller that scopes to a SPECIFIC repo without reading it back from the context.
        write_scope_view(self.storage.connection(), &ScopeContext {
            repo_id: self.active_repo_id.clone(),
            commit_sha: commit_sha.to_string(),
            worktree_id: worktree_id.to_string(),
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
    write_scope_view(conn, &ScopeContext {
        repo_id: repo_id.to_string(),
        commit_sha,
        worktree_id,
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
}

/// Install the scope view on a RAW connection that has no `IndexDatabase` to carry
/// `active_repo_id`, resolving the repo dimension from the connection via
/// [`schema::active_repo_id`] (the sole repo of a single-repo DB, or the placeholder on an
/// un-adopted test DB — matching the placeholder-defaulted `files` rows those tests insert).
/// TEST-ONLY now: the raw-conn hook listeners resolve the repo id explicitly from their config and
/// pass it into [`install_worktree_scope_view`], so only the graph tests (which seed rows under the
/// sole/placeholder repo) reach this config-blind resolver. `IndexDatabase::set_context` likewise
/// passes an explicit repo id (the multi-repo path).
#[cfg(test)]
pub(crate) fn install_scope_view(
    conn: &rusqlite::Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> rusqlite::Result<()> {
    let repo_id = schema::active_repo_id(conn)?;
    write_scope_view(conn, &ScopeContext {
        repo_id,
        commit_sha: commit_sha.to_string(),
        worktree_id: worktree_id.to_string(),
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

    // Every branch (and the shadowing sub-select) is repo-scoped: `worktree_id`/`commit_sha` alone
    // are not globally unique across repos (forks share a commit; the empty base scope is shared),
    // so the `repo_id` predicate is what keeps a sibling repo's rows out of the view.
    conn.execute_batch(
        "
            DROP VIEW IF EXISTS temp.files;
            CREATE TEMP VIEW temp.files AS
            SELECT id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, \
         indexed_revision, commit_sha, worktree_id, has_test_code
            FROM main.files
            WHERE repo_id = (SELECT value FROM temp.connection_context WHERE key = 'repo_id')
              AND worktree_id = (SELECT value FROM temp.connection_context WHERE key = \
         'worktree_id') AND worktree_id != '' AND kind != 'deleted'
            UNION ALL
            SELECT id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, \
         indexed_revision, commit_sha, worktree_id, has_test_code
            FROM main.files
            WHERE repo_id = (SELECT value FROM temp.connection_context WHERE key = 'repo_id')
              AND commit_sha = (SELECT value FROM temp.connection_context WHERE key = 'commit_sha')
              AND commit_sha != ''
              AND path NOT IN (
                  SELECT path FROM main.files
                  WHERE repo_id = (SELECT value FROM temp.connection_context WHERE key = 'repo_id')
                    AND worktree_id = (SELECT value FROM temp.connection_context WHERE key = \
         'worktree_id')
                    AND worktree_id != ''
              );
        ",
    )?;

    Ok(())
}
