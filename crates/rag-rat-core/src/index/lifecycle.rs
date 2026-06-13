use super::*;

impl IndexDatabase {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_graph_check(path, true)
    }

    pub fn database_path(&self) -> &Path {
        self.storage.database_path()
    }

    fn open_with_graph_check(path: &Path, check_graph: bool) -> anyhow::Result<Self> {
        let mut storage = IndexConnection::open(path)?;
        schema::check_compatible(storage.connection())?;
        ai::ensure_model_manifest(storage.connection())?;
        if let Some(root) = meta_for(storage.connection(), "source_root")? {
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
        if let Some(root) = meta_for(storage.connection(), "source_root")? {
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
         indexed_revision, commit_sha, worktree_id, package_id
            FROM main.files
            WHERE worktree_id = (SELECT value FROM temp.connection_context WHERE key = \
         'worktree_id') AND worktree_id != '' AND kind != 'deleted'
            UNION ALL
            SELECT id, path, language, kind, sha256, modified_at_ms, generated, indexed_at_ms, \
         indexed_revision, commit_sha, worktree_id, package_id
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
