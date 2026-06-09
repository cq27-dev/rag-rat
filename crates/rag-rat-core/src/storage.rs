use std::{
    fs,
    path::{Path, PathBuf},
};

use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct StorageStatus {
    pub backend: &'static str,
    pub sqlite_version: String,
    pub fts5_available: bool,
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
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(100))?;
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
        self.conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            -- Wait out a concurrent writer (e.g. the background watcher mid-pass, or a lazy heal
            -- on the query path) instead of failing with SQLITE_BUSY. WAL allows one writer at a
            -- time; this serializes them safely without erroring.
            PRAGMA busy_timeout = 5000;
            ",
        )?;
        Ok(())
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
            crate::index::schema::apply(rw.connection()).unwrap();
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
}
