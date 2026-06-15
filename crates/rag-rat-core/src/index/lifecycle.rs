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

    fn open_with_graph_check(path: &Path, check_graph: bool) -> anyhow::Result<Self> {
        let mut storage = IndexConnection::open(path)?;
        // Forward schema migrations are automatic on open (the index is our own derived data — a
        // binary upgrade must not require a manual `migrate`). Only Newer/Dirty/Missing refuse.
        //
        // An `Older` schema is migrated UNDER THE INDEX WRITE LOCK: auto-migration now fires from
        // ordinary read/MCP opens, so a hot-restarted server and a concurrent `query` could both
        // observe `Older` and race `add_column_if_missing`'s check-then-ALTER into a
        // duplicate-column DDL failure. Serializing on `write_lock_path` makes one opener
        // migrate while the other waits, then re-checks under the lock (the waiter sees
        // `Compatible` and does nothing). Compatible/Newer/Dirty/Missing need no lock —
        // `ensure_compatible_or_migrate` returns or refuses without writing.
        if schema::status(storage.connection())?.state == schema::SchemaState::Older {
            let _lock = crate::locks::FileLock::acquire_timeout(
                &crate::locks::write_lock_path(path),
                SCHEMA_MIGRATE_LOCK_TIMEOUT,
            )?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "timed out waiting for the index write lock to auto-migrate the schema"
                )
            })?;
            schema::ensure_compatible_or_migrate(storage.connection())?;
        } else {
            schema::ensure_compatible_or_migrate(storage.connection())?;
        }
        ai::ensure_model_manifest(storage.connection())?;
        if let Some(root) = read_meta(storage.connection(), "source_root")? {
            storage.set_source_root(PathBuf::from(root));
        }
        let db = Self {
            storage,
            active_commit_sha: String::new(),
            active_worktree_id: String::new(),
            github: github::GitHubContext::default(),
        };
        if check_graph {
            db.ensure_graph_index_current()?;
        }
        Ok(db)
    }

    pub fn open_config(config: &Config) -> anyhow::Result<Self> {
        let mut db = Self::open_with_graph_check(&config.database, false)?;
        db.storage.set_source_root(config.root.clone());
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        db.set_context(&commit_sha, &worktree_id)?;
        // Real usage: resolve the GitHub repo context from the local `gh` CLI here, at the
        // boundary. rebuild/open (used by tests and the bare index command) leave it offline.
        db.github = github::GitHubContext::from_gh();
        db.ensure_graph_index_current()?;
        Ok(db)
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
        if read_meta(storage.connection(), "graph_index_version")?.as_deref()
            != Some(GRAPH_INDEX_VERSION)
        {
            return Ok(None);
        }
        if !ai::model_manifest_is_current(storage.connection())? {
            return Ok(None);
        }
        storage.set_source_root(config.root.clone());
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        let mut db = Self {
            storage,
            active_commit_sha: String::new(),
            active_worktree_id: String::new(),
            github: github::GitHubContext::from_gh(),
        };
        db.set_context(&commit_sha, &worktree_id)?;
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

    pub(super) fn create_or_migrate(path: &Path) -> anyhow::Result<Self> {
        let mut storage = IndexConnection::open(path)?;
        schema::apply(storage.connection())?;
        ai::ensure_model_manifest(storage.connection())?;
        if let Some(root) = read_meta(storage.connection(), "source_root")? {
            storage.set_source_root(PathBuf::from(root));
        }
        Ok(Self {
            storage,
            active_commit_sha: String::new(),
            active_worktree_id: String::new(),
            github: github::GitHubContext::default(),
        })
    }

    pub fn set_context(&mut self, commit_sha: &str, worktree_id: &str) -> anyhow::Result<()> {
        self.active_commit_sha = commit_sha.to_string();
        self.active_worktree_id = worktree_id.to_string();
        install_scope_view(self.storage.connection(), commit_sha, worktree_id)?;
        Ok(())
    }
}

/// Installs the per-connection commit/worktree scoping view; callers query `files` afterward and
/// see only the active context.
pub(crate) fn install_scope_view(
    conn: &rusqlite::Connection,
    commit_sha: &str,
    worktree_id: &str,
) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
            CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);
        ",
    )?;

    let mut stmt =
        conn.prepare("INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES (?1, ?2)")?;
    stmt.execute(params!["commit_sha", commit_sha])?;
    stmt.execute(params!["worktree_id", worktree_id])?;

    conn.execute_batch(
        "
            DROP VIEW IF EXISTS temp.files;
            CREATE TEMP VIEW temp.files AS
            SELECT id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, \
         indexed_revision, commit_sha, worktree_id
            FROM main.files
            WHERE worktree_id = (SELECT value FROM temp.connection_context WHERE key = \
         'worktree_id') AND worktree_id != '' AND kind != 'deleted'
            UNION ALL
            SELECT id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, \
         indexed_revision, commit_sha, worktree_id
            FROM main.files
            WHERE commit_sha = (SELECT value FROM temp.connection_context WHERE key = 'commit_sha')
              AND commit_sha != ''
              AND path NOT IN (
                  SELECT path FROM main.files
                  WHERE worktree_id = (SELECT value FROM temp.connection_context WHERE key = \
         'worktree_id')
                    AND worktree_id != ''
              );
        ",
    )?;

    Ok(())
}
