use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct StorageStatus {
    pub backend: &'static str,
    pub sqlite_version: String,
    pub fts5_available: bool,
}

/// True when `err`'s chain contains a SQLite read-only violation (`SQLITE_READONLY`). The MCP read
/// path opens read tools on a read-only connection (#143), but a few "read" tools still WRITE on a
/// cold path — `semantic_search` heals stale FTS, `read_chunk` heals a stale/deleted file,
/// `git_blame_chunk` fills the blame cache on a miss. That surfaces as this error; the dispatcher
/// uses it to retry the call on a read-write connection (which also performs the heal). Walks the
/// full `anyhow` cause chain because the rusqlite error is wrapped by the time it reaches the
/// caller.
pub fn is_readonly_violation(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ReadOnly
        )
    })
}

/// A `SQLITE_BUSY` / `SQLITE_LOCKED` ("database is locked") failure: a writer (the background
/// watcher, an `index` pass, a lazy heal) held the lock past this connection's `busy_timeout`. The
/// MCP read path's read-write fallback retries on this with bounded backoff (#220) rather than
/// surfacing a `-32603` to the agent. Walks the full `anyhow` cause chain — the rusqlite error is
/// wrapped by the time it reaches the caller, same as [`is_readonly_violation`].
pub fn is_busy(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    })
}

#[derive(Debug)]
pub struct IndexConnection {
    conn: Connection,
    database_path: PathBuf,
    source_root: Option<PathBuf>,
}

impl IndexConnection {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let storage = Self { conn, database_path: path.to_path_buf(), source_root: None };
        storage.setup()?;
        Ok(storage)
    }

    /// Read-only open for latency-critical, never-blocking callers (the grep-augment hook
    /// fallback). Skips `setup()` — no pragma writes, no dir creation — and refuses to create
    /// the file. WAL databases serve concurrent read-only opens; a DB that has never been
    /// opened for write errors here, which callers treat as "no context".
    pub fn open_read_only(path: &Path) -> anyhow::Result<Self> {
        Self::open_read_only_with_busy_timeout(path, std::time::Duration::from_millis(100))
    }

    /// Read-only open that waits out a concurrent writer (the watcher mid-pass, a lazy heal)
    /// instead of failing fast. Used by the MCP read tools: a `SQLITE_OPEN_READ_ONLY` connection
    /// can never acquire the main write lock, so a served read is structurally immune to being
    /// locked out by a writer — and the longer busy_timeout only matters for the brief WAL
    /// checkpoint window. See [#143]. The 100ms `open_read_only` stays fast for the latency-
    /// critical grep-augment hook, which prefers "no context" over blocking.
    pub fn open_read_only_blocking(path: &Path) -> anyhow::Result<Self> {
        Self::open_read_only_with_busy_timeout(path, std::time::Duration::from_secs(5))
    }

    fn open_read_only_with_busy_timeout(
        path: &Path,
        busy_timeout: std::time::Duration,
    ) -> anyhow::Result<Self> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(busy_timeout)?;
        Ok(Self { conn, database_path: path.to_path_buf(), source_root: None })
    }

    /// Read-WRITE open that neither CREATES the database nor WAITS on a busy lock — for the
    /// watcher's non-blocking, side-effect-free out-of-band flush (#658 review). Two departures
    /// from [`open`](Self::open), both load-bearing for that caller:
    /// - `SQLITE_OPEN_READ_WRITE` WITHOUT `_CREATE` (and no `create_dir_all`), so a
    ///   first-time-empty checkout with no index yet ERRORS (`SQLITE_CANTOPEN`) instead of leaving
    ///   a schemaless `.rag-rat/index.sqlite` behind that would poison the friendly no-index read
    ///   path.
    /// - `busy_timeout = 0` and NO `setup()`, so a single write FAILS FAST with `SQLITE_BUSY` under
    ///   a concurrent writer (another repo in a consolidated DB, a checkpoint) instead of stalling
    ///   the caller — the watcher event loop, which must never block on classification/fleet
    ///   triggers — for up to the 5s `setup()` timeout. The WAL journal mode is persistent on an
    ///   already initialized DB, so skipping `setup()`'s `journal_mode = WAL` retry is safe here.
    ///
    /// The caller treats BOTH the open error and a busy write as "skip; the count rides the next
    /// pass" (see [`is_busy`]). NOT for general use — a normal writer wants [`open`](Self::open).
    pub fn open_read_write_no_create_nowait(path: &Path) -> anyhow::Result<Self> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::ZERO)?;
        Ok(Self { conn, database_path: path.to_path_buf(), source_root: None })
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn source_root(&self) -> Option<&Path> {
        self.source_root.as_deref()
    }

    pub fn set_source_root(&mut self, source_root: PathBuf) {
        self.source_root = Some(source_root);
    }

    pub fn execute_batch(&self, sql: &str) -> anyhow::Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    pub fn status(&self) -> anyhow::Result<StorageStatus> {
        let sqlite_version =
            self.conn.query_row("SELECT sqlite_version()", [], |row| row.get::<_, String>(0))?;
        Ok(StorageStatus {
            backend: "sqlite",
            sqlite_version,
            fts5_available: self.fts5_available(),
        })
    }

    fn setup(&self) -> anyhow::Result<()> {
        // busy_timeout is set FIRST so it is already active for `journal_mode = WAL` below: that
        // pragma itself briefly needs the lock and would otherwise fail fast under a concurrent
        // writer (#220). It makes a connection wait out a writer (the watcher mid-pass, a lazy
        // heal) instead of failing with SQLITE_BUSY — WAL allows one writer at a time.
        self.conn.execute_batch(
            "
            PRAGMA busy_timeout = 5000;
            PRAGMA foreign_keys = ON;
            ",
        )?;
        // The delete→WAL transition needs an EXCLUSIVE lock, and SQLite deliberately does NOT run
        // the busy handler on parts of that upgrade path (it returns SQLITE_BUSY immediately to
        // break potential deadlocks among racing upgraders) — so busy_timeout alone does NOT save
        // two truly concurrent FIRST opens of a fresh DB: one fails instantly with "database is
        // locked" (the `concurrent_create_or_migrate_applies_the_schema_exactly_once` race, and a
        // real production shape once a shared multi-repo DB gets its first two writers at once).
        // Bounded manual retry instead: the loser converges as soon as the winner completes,
        // because an ALREADY-WAL database answers `journal_mode = WAL` as a no-op without the
        // exclusive lock. Not a flock: this must cover EVERY open path uniformly (config opens,
        // bare opens, heals), not just the schema-apply bootstrap.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match self.conn.execute_batch(
                "
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                ",
            ) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    let err = anyhow::Error::from(err);
                    if !is_busy(&err) || std::time::Instant::now() >= deadline {
                        return Err(err);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                },
            }
        }
    }

    fn fts5_available(&self) -> bool {
        self.conn
            .execute_batch(
                "
                CREATE VIRTUAL TABLE temp.rag_rat_fts_probe USING fts5(text);
                DROP TABLE temp.rag_rat_fts_probe;
                ",
            )
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_read_only_reads_but_rejects_writes() {
        let dir = std::env::temp_dir().join(format!("ragrat-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("index.db");
        {
            let rw = IndexConnection::open(&db).unwrap();
            crate::schema::apply(rw.connection(), &crate::hooks::MigrationHooks::noop()).unwrap();
        }
        let ro = IndexConnection::open_read_only(&db).unwrap();
        let n: i64 =
            ro.connection().query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0)).unwrap();
        assert_eq!(n, 0);
        let err = ro.connection().execute("INSERT INTO index_meta(key, value) VALUES('x','y')", []);
        assert!(err.is_err(), "read-only connection must reject writes");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_read_only_fails_cleanly_when_database_missing() {
        let missing = std::env::temp_dir().join("ragrat-ro-missing/never-created.db");
        assert!(IndexConnection::open_read_only(&missing).is_err());
    }

    #[test]
    fn open_read_only_blocking_does_not_persist_wal_mode_or_create_sidecars() {
        let dir = std::env::temp_dir().join(format!(
            "ragrat-ro-journal-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("index.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch("PRAGMA journal_mode = DELETE; CREATE TABLE t(v INTEGER);").unwrap();
        }

        {
            let ro = IndexConnection::open_read_only_blocking(&db).unwrap();
            let mode: String =
                ro.connection().query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
            assert_eq!(mode, "delete", "a planning read must not switch the DB to WAL");
        }
        let mode: String = Connection::open(&db)
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete", "journal mode must remain DELETE after the RO open");
        assert!(!db.with_extension("db-wal").exists(), "RO planning must not create a WAL file");
        assert!(!db.with_extension("db-shm").exists(), "RO planning must not create a SHM file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_readonly_violation_flags_only_sqlite_readonly_errors() {
        let dir = std::env::temp_dir().join(format!("ragrat-roviol-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("index.db");
        {
            let rw = IndexConnection::open(&db).unwrap();
            crate::schema::apply(rw.connection(), &crate::hooks::MigrationHooks::noop()).unwrap();
        }
        let ro = IndexConnection::open_read_only(&db).unwrap();

        // A write through a read-only connection → SQLITE_READONLY → flagged (the dispatcher's
        // retry-on-read-write signal, #143 review).
        let write_err: anyhow::Error = ro
            .connection()
            .execute("INSERT INTO index_meta(key, value) VALUES ('x', 'y')", [])
            .unwrap_err()
            .into();
        assert!(is_readonly_violation(&write_err), "a write on a read-only conn must be flagged");

        // A different failure (a syntax error) must NOT be mistaken for a read-only violation, or
        // the dispatcher would mask real errors behind a needless read-write retry.
        let syntax_err: anyhow::Error =
            ro.connection().execute("THIS IS NOT SQL", []).unwrap_err().into();
        assert!(!is_readonly_violation(&syntax_err), "a non-readonly error must not be flagged");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn sqlite_failure(result_code: i32) -> anyhow::Error {
        anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(result_code),
            Some("database is locked".to_string()),
        ))
    }

    #[test]
    fn is_busy_flags_only_busy_and_locked_errors() {
        // SQLITE_BUSY (5) and SQLITE_LOCKED (6) → busy; the MCP read-write fallback retries these.
        assert!(is_busy(&sqlite_failure(rusqlite::ffi::SQLITE_BUSY)));
        assert!(is_busy(&sqlite_failure(rusqlite::ffi::SQLITE_LOCKED)));
        // A read-only violation (8) is NOT busy — it routes to the read-write open, not a backoff
        // retry — and a plain error must not be mistaken for either.
        let ro = sqlite_failure(rusqlite::ffi::SQLITE_READONLY);
        assert!(!is_busy(&ro));
        assert!(is_readonly_violation(&ro));
        assert!(!is_busy(&anyhow::anyhow!("some unrelated error")));
    }
}
