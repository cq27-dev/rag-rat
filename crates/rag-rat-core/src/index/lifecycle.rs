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
        ai::ensure_model_manifest(storage.connection())?;
        let repo_id = schema::single_repo_id(storage.connection())?;
        if let Some(root) = repo_meta(storage.connection(), &repo_id, "source_root")? {
            storage.set_source_root(PathBuf::from(root));
        }
        let db = Self {
            storage,
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
        let mut db = Self::open_with_graph_check(&config.database, false)?;
        db.storage.set_source_root(config.root.clone());
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        db.set_context(&commit_sha, &worktree_id)?;
        // Real usage: resolve the GitHub repo context from the local `gh` CLI here, at the
        // boundary. rebuild/open (used by tests and the bare index command) leave it offline.
        db.github = github::GitHubContext::from_gh();
        db.config = Some(config.clone());
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
        let repo_id = schema::single_repo_id(storage.connection())?;
        if repo_meta(storage.connection(), &repo_id, "graph_index_version")?.as_deref()
            != Some(GRAPH_INDEX_VERSION)
        {
            return Ok(None);
        }
        // A stale generated-flags version owes a re-derive (a write); fall back to read-write so it
        // heals once, after which reads are lock-free again (#202, same posture as the graph gate).
        if read_meta(storage.connection(), GENERATED_FLAGS_VERSION_KEY)?.as_deref()
            != Some(GENERATED_FLAGS_VERSION)
        {
            return Ok(None);
        }
        if !ai::model_manifest_is_current(storage.connection())? {
            return Ok(None);
        }
        // A fresh index owes an active-embedding-model seed from config (a write); fall back to the
        // read-write open so it heals once (#394, same posture as the manifest / graph gates).
        if ai::active_embedding_model_seed_owed(
            storage.connection(),
            config.llm.embedding.backend.model_id(),
        )? {
            return Ok(None);
        }
        storage.set_source_root(config.root.clone());
        let (commit_sha, worktree_id) = resolve_git_context(&config.root);
        let mut db = Self {
            storage,
            active_commit_sha: String::new(),
            active_worktree_id: String::new(),
            github: github::GitHubContext::from_gh(),
            config: Some(config.clone()),
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
        let repo_id = schema::single_repo_id(storage.connection())?;
        if let Some(root) = repo_meta(storage.connection(), &repo_id, "source_root")? {
            storage.set_source_root(PathBuf::from(root));
        }
        Ok(Self {
            storage,
            active_commit_sha: String::new(),
            active_worktree_id: String::new(),
            github: github::GitHubContext::default(),
            config: None,
        })
    }

    pub fn set_context(&mut self, commit_sha: &str, worktree_id: &str) -> anyhow::Result<()> {
        self.active_commit_sha = commit_sha.to_string();
        self.active_worktree_id = worktree_id.to_string();
        install_scope_view(self.storage.connection(), commit_sha, worktree_id)?;
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
/// foreign / unreadable) it is the base scope. `pub` so the CLI hook + the MCP listener (other
/// crates) can scope their context to the worktree the session is actually in (#219).
pub fn install_worktree_scope_view(
    conn: &rusqlite::Connection,
    config_root: &Path,
    cwd: &Path,
) -> anyhow::Result<()> {
    let (commit_sha, worktree_id) = resolve_worktree_scope(config_root, Some(cwd));
    install_scope_view(conn, &commit_sha, &worktree_id)?;
    Ok(())
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
         indexed_revision, commit_sha, worktree_id, has_test_code
            FROM main.files
            WHERE worktree_id = (SELECT value FROM temp.connection_context WHERE key = \
         'worktree_id') AND worktree_id != '' AND kind != 'deleted'
            UNION ALL
            SELECT id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, \
         indexed_revision, commit_sha, worktree_id, has_test_code
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
