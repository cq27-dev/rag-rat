//! Physical health of the shared database file (#482): WAL checkpointing and size/freelist
//! reporting on `IndexDatabase`. The global store is written by every repo's watcher, hooks, and
//! MCP servers, but nothing owned its file hygiene: passive autocheckpoint never truncates the
//! `-wal` (it keeps its high-water mark forever) and freed pages stay in the freelist. The
//! checkpoint here is threshold-gated (a bare `stat` of the sidecar) so quiet watcher passes can
//! attempt it for free; `database_file_health` feeds the `doctor` report.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::*;

/// The pass-tail checkpoint trigger: attempt `wal_checkpoint(TRUNCATE)` only once the sidecar
/// exceeds this. A healthy autocheckpointing WAL stays around 4 MiB (1000 default-size pages), so
/// anything past this is accumulated high-water from a heavy write phase. Below it, the probe is a
/// single `stat` — cheap enough for every quiet watcher pass.
pub const WAL_CHECKPOINT_MIN_BYTES: u64 = 16 * 1024 * 1024;

/// Doctor warning threshold for the `-wal` sidecar: past this, checkpoints are being starved
/// (long-lived readers) or no quiet pass ever runs — worth surfacing rather than silently holding
/// disk.
const WAL_WARN_BYTES: u64 = 64 * 1024 * 1024;

/// Doctor warning threshold for dead space: freelist pages as a fraction of the whole file.
const FREELIST_WARN_FRACTION: f64 = 0.25;

/// Freelist floor below which the fraction never warns — a small database reusing a few dozen
/// pages is healthy churn, not reclaimable waste. 2560 default-size (4 KiB) pages ≈ 10 MiB.
const FREELIST_WARN_MIN_PAGES: i64 = 2_560;

/// What [`IndexDatabase::checkpoint_wal_if_oversized`] did, for the caller's debug logging.
#[derive(Debug, Clone, Serialize)]
pub struct WalCheckpointReport {
    /// Sidecar size at the probe (0 when the file is absent).
    pub wal_bytes_before: u64,
    /// False when the sidecar was under the threshold and nothing ran.
    pub attempted: bool,
    /// True when the checkpoint fully completed and truncated the sidecar (`busy = 0`). False
    /// under concurrent readers/writers that kept frames pinned — the next quiet pass retries.
    pub truncated: bool,
}

/// How long [`IndexDatabase::reclaim_freelist`] waits for the global schema lock before giving up.
/// VACUUM is an explicit operator action, so a generous window lets a quiet moment open up; a
/// timeout surfaces an error rather than blocking forever.
const VACUUM_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What [`IndexDatabase::reclaim_freelist`] reclaimed: the main-file size and freelist before/after
/// the VACUUM, for the operator's report.
#[derive(Debug, Clone, Serialize)]
pub struct FreelistReclaimReport {
    pub main_bytes_before: u64,
    pub main_bytes_after: u64,
    pub freelist_pages_before: i64,
    pub freelist_pages_after: i64,
    /// Whether the post-VACUUM checkpoint truncated the WAL (`busy = 0`). False when a live reader
    /// pinned frames: VACUUM still cleared the freelist, but the compacted image is staged in the
    /// `-wal` and the main file has NOT shrunk on disk yet — stop agents/servers and retry.
    pub wal_truncated: bool,
}

/// Physical health of the database file for the `doctor` report: sizes, freelist share, and the
/// two warning flags with an actionable note. Facts a reader acts on, not internal counters.
#[derive(Debug, Clone, Serialize)]
pub struct DatabaseFileHealth {
    pub main_bytes: u64,
    pub wal_bytes: u64,
    pub page_count: i64,
    pub freelist_pages: i64,
    /// `freelist_pages / page_count` (0 for an empty file).
    pub freelist_fraction: f64,
    /// The `-wal` sidecar exceeds the warn threshold: checkpoints are starved or never attempted.
    pub wal_oversized: bool,
    /// Dead space exceeds the warn fraction (with a floor): a one-off `VACUUM` would reclaim it.
    pub freelist_excessive: bool,
    /// Actionable remedy when either flag is set; `None` otherwise.
    pub note: Option<String>,
}

impl IndexDatabase {
    /// Truncate the WAL sidecar when it has outgrown `min_bytes`; below that, a bare `stat` and
    /// return. Best-effort: `wal_checkpoint(TRUNCATE)` waits for concurrent readers only within
    /// `busy_timeout`, then reports `truncated: false` rather than erroring, and the next quiet
    /// pass retries. Callers must NOT compensate for a busy report by weakening durability —
    /// `synchronous` stays NORMAL on the shared database (the A6 global constraint, #401).
    pub fn checkpoint_wal_if_oversized(
        &self,
        min_bytes: u64,
    ) -> anyhow::Result<WalCheckpointReport> {
        let wal_bytes_before = wal_bytes(self.storage.database_path());
        if wal_bytes_before < min_bytes {
            return Ok(WalCheckpointReport {
                wal_bytes_before,
                attempted: false,
                truncated: false,
            });
        }
        // Row shape: (busy, log, checkpointed). busy = 1 means readers/writers kept the truncate
        // from completing after the busy handler gave up — not an error.
        let busy =
            self.storage
                .connection()
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get::<_, i64>(0))?;
        Ok(WalCheckpointReport { wal_bytes_before, attempted: true, truncated: busy == 0 })
    }

    /// Reclaim dead space by running `VACUUM` — rewrites the file compactly, dropping the whole
    /// freelist. Takes the GLOBAL schema lock, which serializes VACUUM against MIGRATIONS and other
    /// VACUUMs (the other whole-file rewriters). It does NOT exclude ordinary per-repo WRITERS: on
    /// a consolidated DB those take separate per-repo write flocks (`schema_lock_path` is
    /// documented as disjoint from `write_lock_path`) and write concurrently by design — there
    /// is no single lock they all honor. So VACUUM relies on SQLite's own file locking and
    /// FAILS with a busy error if a writer (a watcher/`index` for ANY repo, or an MCP server
    /// holding a read txn) is active mid-run — no corruption, just a clean refusal telling the
    /// operator to quiesce agents and retry. An explicit operator action (`doctor --vacuum`),
    /// never automatic: it can rewrite hundreds of MB.
    pub fn reclaim_freelist(&self) -> anyhow::Result<FreelistReclaimReport> {
        let path = self.storage.database_path().to_path_buf();
        let _lock = crate::locks::WriteLock::acquire_schema_timeout(&path, VACUUM_LOCK_TIMEOUT)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "timed out waiting for the schema lock to VACUUM the database — another \
                     rag-rat writer is active; stop agents/watchers and retry"
                )
            })?;
        // Snapshot under the lock so `before` can't drift against a concurrent writer.
        let before = self.database_file_health()?;
        // VACUUM on a FRESH bare connection, not `self`: a scoped `IndexDatabase` installs a
        // `temp.files` TEMP VIEW (`install_scope_view`), and VACUUM rejects a connection carrying a
        // temp view ("views may not be indexed"). A pragma'd bare open matches the manual
        // `sqlite3 <db> VACUUM` remedy and rewrites the same file; `self` sees the compacted result
        // on its next read.
        let vacuum_conn = crate::storage::IndexConnection::open(&path)?;
        // A concurrent writer (any repo's watcher/index, or a reader holding a txn) makes VACUUM
        // fail busy — the schema lock doesn't hold those off. Turn a raw SQLITE_BUSY into an
        // actionable refusal (no corruption, just retry once quiet).
        if let Err(err) = vacuum_conn.connection().execute_batch("VACUUM") {
            let err = anyhow::Error::from(err);
            return Err(if crate::storage::is_busy(&err) {
                anyhow::anyhow!(
                    "VACUUM could not get exclusive access to the database — a rag-rat writer (a \
                     watcher/index for any repo, or an active reader) is running. Stop \
                     agents/watchers/MCP servers and re-run `rag-rat doctor --vacuum`."
                )
            } else {
                err.context("VACUUM failed")
            });
        }
        // VACUUM may RENUMBER git_commits' rowids — it has no explicit INTEGER PRIMARY KEY (keyed
        // by `hash` / `(repo_id, hash)`), and SQLite documents that VACUUM can change
        // rowids for such tables. That desyncs the external-content `commit_fts`
        // (content='git_commits', content_rowid='rowid'), so `commit_search` would return
        // wrong/missing commits. Rebuild it against the post-VACUUM rowids. It is the ONLY
        // at-risk FTS: `chunk_fts` is contentless and `github_fts`/`repo_memory_fts` are
        // standalone (not rowid-linked).
        crate::index::schema::rebuild_commit_fts(vacuum_conn.connection())?;
        // In WAL mode VACUUM's compaction lands in the `-wal`; fold it back and truncate the
        // sidecar so the main file physically shrinks (and doesn't just move dead space to the
        // WAL). The checkpoint returns (busy, log, checkpointed): busy = 1 means a reader
        // pinned frames and the truncate could not complete — surface it (`wal_truncated`)
        // rather than swallow it, or `doctor --vacuum` would report success while the file
        // never shrank on disk.
        let busy =
            vacuum_conn
                .connection()
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get::<_, i64>(0))?;
        drop(vacuum_conn);
        let after = self.database_file_health()?;
        Ok(FreelistReclaimReport {
            main_bytes_before: before.main_bytes,
            main_bytes_after: after.main_bytes,
            freelist_pages_before: before.freelist_pages,
            freelist_pages_after: after.freelist_pages,
            wal_truncated: busy == 0,
        })
    }

    /// Size and dead-space facts about the database file, with warning flags at the default
    /// thresholds. Read-only; feeds the `doctor` report.
    pub fn database_file_health(&self) -> anyhow::Result<DatabaseFileHealth> {
        self.database_file_health_at(
            WAL_WARN_BYTES,
            FREELIST_WARN_FRACTION,
            FREELIST_WARN_MIN_PAGES,
        )
    }

    /// [`Self::database_file_health`] with the thresholds injected (tests).
    pub(crate) fn database_file_health_at(
        &self,
        wal_warn_bytes: u64,
        freelist_warn_fraction: f64,
        freelist_warn_min_pages: i64,
    ) -> anyhow::Result<DatabaseFileHealth> {
        let conn = self.storage.connection();
        let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let freelist_pages: i64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        let main_bytes =
            std::fs::metadata(self.storage.database_path()).map(|meta| meta.len()).unwrap_or(0);
        let wal_bytes = wal_bytes(self.storage.database_path());
        let freelist_fraction =
            if page_count > 0 { freelist_pages as f64 / page_count as f64 } else { 0.0 };
        let wal_oversized = wal_bytes > wal_warn_bytes;
        let freelist_excessive =
            freelist_pages >= freelist_warn_min_pages && freelist_fraction > freelist_warn_fraction;
        let note = match (wal_oversized, freelist_excessive) {
            (false, false) => None,
            (wal, freelist) => {
                let mut parts = Vec::new();
                if wal {
                    parts.push(
                        "the -wal sidecar is oversized — a quiet watcher pass truncates it, or \
                         long-lived readers are starving checkpoints",
                    );
                }
                if freelist {
                    parts.push(
                        "dead space is excessive — reclaim it with `rag-rat doctor --vacuum` (a \
                         one-off VACUUM that rewrites the file; best run while agents/watchers \
                         are quiet)",
                    );
                }
                Some(parts.join("; "))
            },
        };
        Ok(DatabaseFileHealth {
            main_bytes,
            wal_bytes,
            page_count,
            freelist_pages,
            freelist_fraction,
            wal_oversized,
            freelist_excessive,
            note,
        })
    }
}

/// Size of the `-wal` sidecar, 0 when absent. SQLite derives the sidecar name by appending
/// `-wal` to the database path byte-for-byte, so build it the same way (no extension juggling).
fn wal_bytes(database: &Path) -> u64 {
    let mut sidecar = database.as_os_str().to_os_string();
    sidecar.push("-wal");
    std::fs::metadata(PathBuf::from(sidecar)).map(|meta| meta.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Minimal single-file fixture: file-health tests need a real WAL-mode database with a
    /// registered repo, not any particular indexed content.
    fn fixture_config(tag: &str) -> crate::Config {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("rag-rat-file-health-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn health_probe() -> i32 { 1 }\n").unwrap();
        crate::Config {
            trackers: Vec::new(),
            papertrail: Default::default(),
            repo_id_override: None,
            database_key_pinned: true,
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![crate::config::ResolvedTarget {
                name: "rust".to_string(),
                language: crate::language::Language::Rust,
                directories: vec![std::path::PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: crate::config::TargetKind::Source,
            }],
            llm: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
            search: Default::default(),
            memory: Default::default(),
            log: Default::default(),
            source_root_reanchored_from: None,
            allow_empty: false,
        }
    }

    fn build_fixture(tag: &str) -> crate::IndexDatabase {
        crate::IndexDatabase::rebuild(&fixture_config(tag)).unwrap()
    }

    /// Guarantee at least one un-checkpointed WAL frame (the rebuild may have autocheckpointed).
    fn write_a_wal_frame(db: &crate::IndexDatabase) {
        db.storage
            .execute_batch(
                "INSERT INTO index_meta(key, value) VALUES('file_health_probe', '1')
                 ON CONFLICT(key) DO UPDATE SET value = value || '1'",
            )
            .unwrap();
    }

    #[test]
    fn checkpoint_skips_when_wal_is_under_the_threshold() {
        let db = build_fixture("skip");
        write_a_wal_frame(&db);
        let before = wal_bytes(db.storage.database_path());
        assert!(before > 0, "fixture must have WAL content for the skip to be meaningful");

        let report = db.checkpoint_wal_if_oversized(u64::MAX).unwrap();
        assert!(!report.attempted, "under the threshold the checkpoint must not run");
        assert!(!report.truncated);
        assert_eq!(report.wal_bytes_before, before);
        assert_eq!(
            wal_bytes(db.storage.database_path()),
            before,
            "a skipped checkpoint must leave the WAL untouched"
        );
    }

    #[test]
    fn checkpoint_truncates_an_oversized_wal_to_zero() {
        let db = build_fixture("truncate");
        write_a_wal_frame(&db);
        assert!(wal_bytes(db.storage.database_path()) > 0);

        let report = db.checkpoint_wal_if_oversized(1).unwrap();
        assert!(report.attempted);
        assert!(report.truncated, "no concurrent readers → TRUNCATE must complete");
        assert!(report.wal_bytes_before > 0);
        assert_eq!(
            wal_bytes(db.storage.database_path()),
            0,
            "TRUNCATE resets the WAL high-water mark to an empty sidecar"
        );
    }

    /// Free a deterministic batch of pages into the main-file freelist: stage a scratch table,
    /// fill it, drop it, then checkpoint so the freed pages land in `main` (not just the WAL) — so
    /// `main_bytes` reflects the dead space and a VACUUM's shrink is observable.
    fn seed_dead_space(db: &crate::IndexDatabase) {
        db.storage
            .execute_batch(
                "CREATE TABLE file_health_scratch(x BLOB);
                 WITH RECURSIVE cnt(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM cnt WHERE i < 200)
                 INSERT INTO file_health_scratch SELECT randomblob(4096) FROM cnt;
                 DROP TABLE file_health_scratch;",
            )
            .unwrap();
        db.checkpoint_wal_if_oversized(1).unwrap();
    }

    #[test]
    fn reclaim_freelist_drops_dead_space_and_shrinks_the_file() {
        let db = build_fixture("reclaim");
        seed_dead_space(&db);
        let before = db.database_file_health().unwrap();
        assert!(before.freelist_pages > 0, "fixture must have dead pages to reclaim");

        let report = db.reclaim_freelist().unwrap();

        let after = db.database_file_health().unwrap();
        assert_eq!(after.freelist_pages, 0, "VACUUM reclaims the whole freelist");
        assert!(
            after.main_bytes < before.main_bytes,
            "reclaimed pages shrink the file: {} -> {}",
            before.main_bytes,
            after.main_bytes
        );
        assert_eq!(report.freelist_pages_before, before.freelist_pages);
        assert_eq!(report.freelist_pages_after, 0);
        assert_eq!(report.main_bytes_before, before.main_bytes);
        assert!(report.main_bytes_after < report.main_bytes_before);
        assert!(
            report.wal_truncated,
            "no concurrent reader → the post-VACUUM checkpoint truncates"
        );
    }

    #[test]
    fn reclaim_freelist_reports_a_busy_checkpoint_without_shrinking() {
        use rusqlite::TransactionBehavior;

        let db = build_fixture("reclaim-busy");
        seed_dead_space(&db);
        let before = db.database_file_health().unwrap();
        assert!(before.freelist_pages > 0);

        // A second connection holds an open READ transaction over the file for the whole reclaim,
        // pinning WAL frames so the post-VACUUM `wal_checkpoint(TRUNCATE)` reports busy = 1. VACUUM
        // itself still completes (WAL readers don't block the writer), so the freelist clears, but
        // the main file cannot be truncated while the reader is live.
        let mut reader = rusqlite::Connection::open(db.storage.database_path()).unwrap();
        let read_txn = reader.transaction_with_behavior(TransactionBehavior::Deferred).unwrap();
        read_txn
            .query_row("SELECT count(*) FROM main.files", [], |row| row.get::<_, i64>(0))
            .unwrap();

        let report = db.reclaim_freelist().unwrap();

        assert!(!report.wal_truncated, "a pinned reader must leave the checkpoint busy");
        assert_eq!(report.freelist_pages_after, 0, "VACUUM still clears the freelist");
        assert_eq!(
            report.main_bytes_after, report.main_bytes_before,
            "a busy checkpoint means the file did not shrink on disk"
        );
        drop(read_txn);
    }

    #[test]
    fn reclaim_freelist_refuses_when_a_writer_holds_the_database() {
        use rusqlite::TransactionBehavior;

        let db = build_fixture("reclaim-writer");
        seed_dead_space(&db);

        // A second connection holds a write transaction (BEGIN IMMEDIATE = RESERVED lock) for the
        // whole reclaim: the schema lock doesn't hold off writers, so VACUUM can't get exclusive
        // access and fails busy. The refusal must be actionable, not a raw SQLITE_BUSY.
        let mut writer = rusqlite::Connection::open(db.storage.database_path()).unwrap();
        let write_txn = writer.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();

        let err = db.reclaim_freelist().expect_err("a held writer must make VACUUM refuse");
        assert!(
            err.to_string().contains("doctor --vacuum"),
            "the refusal names the retry remedy: {err}"
        );
        drop(write_txn);
    }

    #[test]
    fn reclaim_freelist_resyncs_commit_fts_after_vacuum() {
        use rusqlite::OptionalExtension;

        let db = build_fixture("reclaim-commitfts");
        // git_commits has no INTEGER PRIMARY KEY, so VACUUM can renumber its rowids and desync the
        // external-content `commit_fts`. Insert a commit but DON'T sync the FTS — reclaim must
        // rebuild it, or `commit_search` (which joins `git_commits.rowid = commit_fts.rowid`) is
        // wrong after `doctor --vacuum`.
        db.storage
            .execute_batch(
                "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s, \
                 committed_at_s, subject, body, repo_id) VALUES ('deadbeefcafe', 'a', 'a@x', 1, \
                 1, 'vacuumtoken subject', '', (SELECT repo_id FROM repos LIMIT 1));",
            )
            .unwrap();
        seed_dead_space(&db);

        db.reclaim_freelist().unwrap();

        let hash: Option<String> = db
            .storage
            .connection()
            .query_row(
                "SELECT gc.hash FROM commit_fts JOIN git_commits gc ON gc.rowid = \
                 commit_fts.rowid WHERE commit_fts MATCH 'vacuumtoken'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(
            hash.as_deref(),
            Some("deadbeefcafe"),
            "reclaim must rebuild commit_fts in sync with git_commits' post-VACUUM rowids"
        );
    }

    #[test]
    fn file_health_stays_quiet_on_a_healthy_database() {
        let db = build_fixture("healthy");
        let health = db.database_file_health().unwrap();
        assert!(health.main_bytes > 0);
        assert!(health.page_count > 0);
        assert!(
            (0.0..=1.0).contains(&health.freelist_fraction),
            "fraction is a ratio, got {}",
            health.freelist_fraction
        );
        assert!(!health.wal_oversized, "a fresh fixture is nowhere near the WAL warn threshold");
        assert!(!health.freelist_excessive);
        assert!(health.note.is_none(), "no advisory when nothing is wrong");
    }

    #[test]
    fn file_health_flags_fire_with_injected_thresholds() {
        let db = build_fixture("flags");
        write_a_wal_frame(&db);
        // Free a deterministic batch of pages: stage a scratch table (in `main` — a temp table
        // lives in a separate temp database and frees nothing here), then drop it.
        db.storage
            .execute_batch(
                "CREATE TABLE file_health_scratch(x BLOB);
                 WITH RECURSIVE cnt(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM cnt WHERE i < 200)
                 INSERT INTO file_health_scratch SELECT randomblob(4096) FROM cnt;
                 DROP TABLE file_health_scratch;",
            )
            .unwrap();

        let health = db.database_file_health_at(1, 0.0, 1).unwrap();
        assert!(health.wal_bytes > 1, "the meta write and drop must have landed in the WAL");
        assert!(health.wal_oversized, "wal_warn_bytes=1 must flag any non-empty WAL");
        assert!(health.freelist_pages > 0, "dropping the scratch table must free pages");
        assert!(health.freelist_excessive, "zero thresholds must flag any freelist");
        let note = health.note.expect("an advisory accompanies raised flags");
        assert!(note.contains("VACUUM"), "the freelist remedy names VACUUM: {note}");
    }
}
