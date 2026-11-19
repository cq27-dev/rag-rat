use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::functions::FunctionFlags;
use rusqlite::{Transaction, TransactionBehavior};

use super::*;

thread_local! {
    static BUSY_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
    static FLUSH_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
}

fn trace_flush_attempts(conn: &Connection) {
    unsafe extern "C" fn count_begin(
        _event: u32,
        _context: *mut std::ffi::c_void,
        _statement: *mut std::ffi::c_void,
        sql: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: SQLITE_TRACE_STMT supplies a valid, nul-terminated SQL string.
        let sql = unsafe { std::ffi::CStr::from_ptr(sql.cast()) };
        if sql == c"BEGIN IMMEDIATE" {
            FLUSH_ATTEMPTS.set(FLUSH_ATTEMPTS.get().saturating_add(1));
        }
        0
    }

    FLUSH_ATTEMPTS.set(0);
    // SAFETY: the connection is live, the callback keeps no pointers, and its thread-local
    // counter outlives this connection. SQLite invokes it synchronously on this test's thread.
    let result = unsafe {
        rusqlite::ffi::sqlite3_trace_v2(
            conn.handle(),
            rusqlite::ffi::SQLITE_TRACE_STMT,
            Some(count_begin),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(result, rusqlite::ffi::SQLITE_OK);
}

struct AttemptLimit<'a> {
    conn: &'a Connection,
}

impl<'a> AttemptLimit<'a> {
    fn assert_escape_interrupts_fourth_begin(conn: &'a Connection) {
        // Exercise the escape with a finite probe while the other connection holds its lock.
        // This checks the callback wiring before trusting it to stop a retry regression.
        let limit = Self::new(conn);
        for attempt in 1..=4 {
            let error = conn.execute_batch("BEGIN IMMEDIATE").unwrap_err();
            let expected = if attempt <= 3 {
                rusqlite::ffi::SQLITE_BUSY
            } else {
                rusqlite::ffi::SQLITE_INTERRUPT
            };
            assert!(
                matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.extended_code == expected),
                "attempt-limit escape returned the wrong error on attempt {attempt}"
            );
        }
        drop(limit);
        assert!(conn.is_autocommit());
        BUSY_ATTEMPTS.set(0);
    }

    fn new(conn: &'a Connection) -> Self {
        unsafe extern "C" fn interrupt_extra_attempt(_context: *mut std::ffi::c_void) -> i32 {
            i32::from(FLUSH_ATTEMPTS.get() > 3)
        }

        trace_flush_attempts(conn);
        // SAFETY: the live connection owns these synchronous callbacks, which retain no
        // pointers. Returning nonzero interrupts SQL; assertions run after SQLite returns.
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                conn.handle(),
                1,
                Some(interrupt_extra_attempt),
                std::ptr::null_mut(),
            );
        }
        Self { conn }
    }

    fn assert_exhausted(self) {
        assert_eq!(FLUSH_ATTEMPTS.get(), 3, "batch retry must stop after three attempts");
    }
}

impl Drop for AttemptLimit<'_> {
    fn drop(&mut self) {
        // SAFETY: the connection outlives this guard. Disable callbacks before later queries
        // or cleanup, including when assert_exhausted unwinds after an interrupted attempt.
        unsafe {
            rusqlite::ffi::sqlite3_progress_handler(
                self.conn.handle(),
                0,
                None,
                std::ptr::null_mut(),
            );
            rusqlite::ffi::sqlite3_trace_v2(self.conn.handle(), 0, None, std::ptr::null_mut());
        }
    }
}

struct Fixture {
    conn: Connection,
    writer: Connection,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clones.sqlite");
        let conn = Connection::open(&path).unwrap();
        rag_rat_db::schema::apply(&conn, &rag_rat_db::hooks::MigrationHooks::noop()).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;").unwrap();
        seed_building_generation(&conn);
        let writer = Connection::open(path).unwrap();
        writer.busy_timeout(Duration::ZERO).unwrap();
        BUSY_ATTEMPTS.set(0);
        conn.busy_handler(Some(|_| {
            BUSY_ATTEMPTS.set(BUSY_ATTEMPTS.get().saturating_add(1));
            false
        }))
        .unwrap();
        Self { conn, writer, _directory: directory }
    }

    fn lock_writer(&self) -> Transaction<'_> {
        Transaction::new_unchecked(&self.writer, TransactionBehavior::Immediate).unwrap()
    }
}

fn seed_building_generation(conn: &Connection) {
    conn.execute(
        "INSERT INTO clone_graph_generations
         (generation, status, theta_floor, normalizer_kind, normalizer_version, source_revision,
          cursor_symbol_id, edges_written, postings_written, started_at_ms)
         VALUES (1, 'Building', 0.8, 'baseline', ?1, 'revision', 7, 0, 1, 123)",
        [NORM_VERSION],
    )
    .unwrap();
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    edges: i64,
    postings: i64,
    generation: (String, i64, i64, String, i64, i64),
}

fn read_build_checkpoint(conn: &Connection) -> Snapshot {
    Snapshot {
        edges: conn.query_row("SELECT COUNT(*) FROM clone_edges", [], |r| r.get(0)).unwrap(),
        postings: conn
            .query_row("SELECT COUNT(*) FROM clone_subblock_postings", [], |r| r.get(0))
            .unwrap(),
        generation: conn
            .query_row(
                "SELECT status, cursor_symbol_id, edges_written, source_revision, \
                 postings_written,
                        started_at_ms FROM clone_graph_generations WHERE generation = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap(),
    }
}

fn pending_batch() -> (Vec<EdgeRow>, Vec<PostingGroup>) {
    let a = ("a.rs".to_owned(), 0, "sha-a".to_owned());
    let b = ("b.rs".to_owned(), 0, "sha-b".to_owned());
    (vec![make_edge(&a, 10, &b, 10, 10, 1.0, "struct_hash")], vec![PostingGroup {
        anchor: a,
        tokens: vec![11, 22],
    }])
}

fn flush<'a>(
    conn: &'a Connection,
    batch: &'a mut Vec<EdgeRow>,
    postings: &'a mut Vec<PostingGroup>,
) -> BatchFlush<'a> {
    BatchFlush { conn, generation: 1, batch, postings, cursor: 42, cumulative_edges: 0 }
}

fn assert_retry_exhausts_within_wait_budget(fixture: &Fixture) {
    // Success after a writer releases does not distinguish a bounded iterator from cycle().
    // Check exhaustion explicitly, with a test-owned bound that fails inside the Rust sleeper.
    let before = read_build_checkpoint(&fixture.writer);
    let lock = fixture.lock_writer();
    let (mut batch, mut postings) = pending_batch();
    BUSY_ATTEMPTS.set(0);
    let mut waits = 0;
    let result = flush(&fixture.conn, &mut batch, &mut postings).flush(BatchRetry {
        backoff: BatchRetry::default().backoff,
        sleep: |_| {
            waits += 1;
            assert!(waits <= 2, "batch retry exceeded two permitted waits");
        },
    });
    drop(lock);
    assert!(result.is_err(), "held writer must exhaust retries");
    assert!(is_sqlite_busy(&result.unwrap_err()));
    assert_eq!((BUSY_ATTEMPTS.get(), waits), (3, 2));
    assert_eq!(read_build_checkpoint(&fixture.writer), before);
    assert_eq!((batch.len(), postings.len()), (1, 1));
    assert!(fixture.conn.is_autocommit());
}

#[test]
fn batch_busy_retry_commits_once_after_writer_releases() {
    let fixture = Fixture::new();
    let (mut batch, mut postings) = pending_batch();
    // A clean production flush provides the persisted baseline for exactly one batch.
    assert_eq!(flush_batch(&fixture.conn, 1, &mut batch, &mut postings, 42, 0).unwrap(), 1);
    let clean = read_build_checkpoint(&fixture.writer);
    assert_eq!(
        (clean.edges, clean.postings, clean.generation.1, clean.generation.2),
        (1, 2, 42, 1)
    );
    fixture.conn.execute_batch("DELETE FROM clone_graph_generations").unwrap();
    seed_building_generation(&fixture.conn);
    let before = read_build_checkpoint(&fixture.writer);
    let (mut batch, mut postings) = pending_batch();
    let mut lock = Some(fixture.lock_writer());
    let mut waits = Vec::new();
    let result = flush(&fixture.conn, &mut batch, &mut postings).flush(BatchRetry {
        backoff: BatchRetry::default().backoff,
        sleep: |delay| {
            waits.push(delay);
            assert!(waits.len() <= 1, "released-writer flush exceeded one permitted wait");
            assert_eq!(read_build_checkpoint(&fixture.writer), before);
            // Explicit retry/writer handshake; dropping the guard also releases on panic.
            lock.take().unwrap().rollback().unwrap();
        },
    });
    drop(lock);
    assert!(result.is_ok(), "a released writer must allow the batch retry: {result:?}");
    assert_eq!(result.unwrap(), 1);
    assert_eq!(BUSY_ATTEMPTS.get(), 1, "the flush must encounter real SQLITE_BUSY");
    assert_eq!(waits, [Duration::from_millis(50)]);
    assert_eq!(read_build_checkpoint(&fixture.writer), clean);
    assert!(batch.is_empty() && postings.is_empty(), "committed buffers must clear");
    assert_retry_exhausts_within_wait_budget(&fixture);
}

#[test]
fn batch_busy_retry_exhaustion_preserves_checkpoint_and_buffers() {
    let fixture = Fixture::new();
    let before = read_build_checkpoint(&fixture.writer);
    let lock = fixture.lock_writer();
    let (mut batch, mut postings) = pending_batch();
    let mut waits = Vec::new();
    let result = flush(&fixture.conn, &mut batch, &mut postings).flush(BatchRetry {
        backoff: BatchRetry::default().backoff,
        sleep: |delay| {
            waits.push(delay);
            assert!(waits.len() <= 2, "batch retry exceeded two permitted waits");
        },
    });
    drop(lock);
    assert!(result.is_err(), "an exhausted batch must still fail");
    let error = result.unwrap_err();
    assert!(rag_rat_db::storage::is_busy(&error));
    assert!(
        matches!(
            error.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::DatabaseBusy && code.extended_code == 5
        ),
        "classify the actual anyhow-wrapped SQLite error: {error:?}"
    );
    assert_eq!(BUSY_ATTEMPTS.get(), 3, "retry must stop after three busy attempts");
    assert_eq!(waits, [Duration::from_millis(50), Duration::from_millis(100)]);
    assert_eq!(read_build_checkpoint(&fixture.writer), before);
    assert_eq!((batch.len(), postings.len(), postings[0].tokens.len()), (1, 1, 2));
    assert!(fixture.conn.is_autocommit());
}

fn reject_checkpoint_writes(conn: &Connection) -> Arc<AtomicUsize> {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    conn.create_scalar_function("checkpoint_attempt", 0, FunctionFlags::SQLITE_UTF8, move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(0)
    })
    .unwrap();
    conn.execute_batch(
        "CREATE TEMP TRIGGER reject_checkpoint BEFORE UPDATE ON clone_graph_generations
         BEGIN SELECT checkpoint_attempt(); SELECT RAISE(ABORT, 'checkpoint rejected'); END;",
    )
    .unwrap();
    attempts
}

#[test]
fn batch_busy_retry_returns_first_non_busy_error() {
    let fixture = Fixture::new();
    let before = read_build_checkpoint(&fixture.writer);
    let checkpoint_attempts = reject_checkpoint_writes(&fixture.conn);
    let (mut batch, mut postings) = pending_batch();
    let first = flush_batch(&fixture.conn, 1, &mut batch, &mut postings, 42, 0);
    assert!(first.is_err());
    assert_eq!(checkpoint_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(read_build_checkpoint(&fixture.writer), before);
    assert_eq!((batch.len(), postings.len()), (1, 1));
    checkpoint_attempts.store(0, Ordering::SeqCst);
    // Also exercise the terminal error after one BUSY, with retry budget remaining.
    let mut lock = Some(fixture.lock_writer());
    let mut waits = 0;
    let result = flush(&fixture.conn, &mut batch, &mut postings).flush(BatchRetry {
        backoff: BatchRetry::default().backoff,
        sleep: |_| {
            waits += 1;
            assert!(waits <= 1, "terminal-error flush exceeded one permitted wait");
            if let Some(lock) = lock.take() {
                lock.rollback().unwrap();
            }
        },
    });
    drop(lock);
    assert_eq!(
        checkpoint_attempts.load(Ordering::SeqCst),
        1,
        "the first non-BUSY write must surface immediately"
    );
    let error = result.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::ConstraintViolation
    ));
    assert_eq!((BUSY_ATTEMPTS.get(), waits), (1, 1));
    assert_eq!(read_build_checkpoint(&fixture.writer), before, "earlier inserts must roll back");
    assert_eq!((batch.len(), postings.len()), (1, 1));
    assert!(fixture.conn.is_autocommit());
    assert_retry_exhausts_within_wait_budget(&fixture);
}

#[test]
fn batch_busy_retry_uses_injected_backoff_before_last_attempt() {
    let fixture = Fixture::new();
    let before = read_build_checkpoint(&fixture.writer);
    let mut lock = Some(fixture.lock_writer());
    let (mut batch, mut postings) = pending_batch();
    let mut elapsed = Duration::ZERO;
    let mut waits = 0;
    let result = flush(&fixture.conn, &mut batch, &mut postings).flush(BatchRetry {
        backoff: BatchRetry::default().backoff,
        sleep: |delay| {
            waits += 1;
            assert!(waits <= 2, "batch retry exceeded two permitted waits");
            assert!(fixture.conn.is_autocommit(), "a failed attempt must release its transaction");
            assert_eq!(read_build_checkpoint(&fixture.writer), before);
            elapsed += delay;
            match elapsed.as_millis() {
                50 => {},
                150 => lock.take().unwrap().rollback().unwrap(),
                _ => panic!("unexpected backoff: {elapsed:?}"),
            }
        },
    });
    drop(lock);
    assert!(result.is_ok(), "the last bounded attempt must commit: {result:?}");
    assert_eq!(result.unwrap(), 1, "only committed edge inserts count");
    assert_eq!(BUSY_ATTEMPTS.get(), 2);
    assert_eq!(elapsed, Duration::from_millis(150));
    let after = read_build_checkpoint(&fixture.writer);
    assert_eq!(
        (after.edges, after.postings, after.generation.1, after.generation.2),
        (1, 2, 42, 1)
    );
    assert!(batch.is_empty() && postings.is_empty());
    assert_retry_exhausts_within_wait_budget(&fixture);
}

#[test]
fn batch_busy_retry_default_sleeper_waits_for_backoff() {
    let fixture = Fixture::new();
    let before = read_build_checkpoint(&fixture.writer);
    let lock = fixture.lock_writer();
    let (mut batch, mut postings) = pending_batch();
    // Observe the real default backend. Only a lower bound is required: scheduler delays
    // cannot fail this assertion, and lock ownership does not depend on elapsed time.
    let retry = BatchRetry::default();
    let minimum_wait: Duration = retry.backoff.into_iter().sum();
    AttemptLimit::assert_escape_interrupts_fourth_begin(&fixture.conn);
    let attempt_limit = AttemptLimit::new(&fixture.conn);
    let started = Instant::now();
    let result = flush(&fixture.conn, &mut batch, &mut postings).flush(retry);
    let elapsed = started.elapsed();
    drop(lock);
    attempt_limit.assert_exhausted();
    assert!(result.is_err());
    assert_eq!(BUSY_ATTEMPTS.get(), 3, "the default sleeper must reach the bounded final attempt");
    assert!(
        elapsed >= minimum_wait,
        "default sleeper skipped backoff: {elapsed:?} < {minimum_wait:?}"
    );
    assert_eq!(read_build_checkpoint(&fixture.writer), before);
    assert_eq!((batch.len(), postings.len()), (1, 1));
}

#[test]
fn batch_busy_retry_production_entrypoint_exhausts_with_default_backoff() {
    let fixture = Fixture::new();
    let before = read_build_checkpoint(&fixture.writer);
    let lock = fixture.lock_writer();
    let (mut batch, mut postings) = pending_batch();
    AttemptLimit::assert_escape_interrupts_fourth_begin(&fixture.conn);
    let attempt_limit = AttemptLimit::new(&fixture.conn);
    let started = Instant::now();
    let result = flush_batch(&fixture.conn, 1, &mut batch, &mut postings, 42, 0);
    let elapsed = started.elapsed();
    drop(lock);
    attempt_limit.assert_exhausted();
    assert!(result.is_err(), "held writer must exhaust production retries");
    assert!(is_sqlite_busy(&result.unwrap_err()));
    assert_eq!(BUSY_ATTEMPTS.get(), 3, "production flush must encounter three real BUSYs");
    assert!(
        elapsed >= Duration::from_millis(150),
        "production flush skipped default backoff: {elapsed:?} < 150ms"
    );
    assert_eq!(read_build_checkpoint(&fixture.writer), before);
    assert_eq!((batch.len(), postings.len(), postings[0].tokens.len()), (1, 1, 2));
    assert!(fixture.conn.is_autocommit());
}

#[test]
fn batch_busy_retry_rolls_back_failed_commit_before_repeating_batch() {
    let fixture = Fixture::new();
    // A rollback-journal reader lets BEGIN IMMEDIATE succeed but makes COMMIT busy.
    fixture.conn.execute_batch("PRAGMA journal_mode = DELETE").unwrap();
    let before = read_build_checkpoint(&fixture.writer);
    let mut reader = Some(fixture.writer.unchecked_transaction().unwrap());
    assert_eq!(read_build_checkpoint(reader.as_ref().unwrap()), before);
    let (mut batch, mut postings) = pending_batch();
    let mut waits = 0;
    let result = flush(&fixture.conn, &mut batch, &mut postings).flush(BatchRetry {
        backoff: BatchRetry::default().backoff,
        sleep: |_| {
            waits += 1;
            assert!(waits <= 1, "busy-COMMIT flush exceeded one permitted wait");
            assert!(fixture.conn.is_autocommit(), "failed COMMIT must roll back before waiting");
            assert_eq!(read_build_checkpoint(reader.as_ref().unwrap()), before);
            reader.take().unwrap().rollback().unwrap();
        },
    });
    drop(reader);
    assert!(result.is_ok(), "retry must recover from a busy COMMIT: {result:?}");
    assert_eq!(result.unwrap(), 1, "rolled-back edge inserts must not count");
    assert_eq!((BUSY_ATTEMPTS.get(), waits), (1, 1));
    let after = read_build_checkpoint(&fixture.writer);
    assert_eq!(
        (after.edges, after.postings, after.generation.1, after.generation.2),
        (1, 2, 42, 1)
    );
    assert!(batch.is_empty() && postings.is_empty());
    assert_retry_exhausts_within_wait_budget(&fixture);
}

#[test]
fn batch_busy_retry_sqlite_locked_is_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("locked.sqlite");
    let flags = rusqlite::OpenFlags::default() | rusqlite::OpenFlags::SQLITE_OPEN_SHARED_CACHE;
    let conn = Connection::open_with_flags(&path, flags).unwrap();
    rag_rat_db::schema::apply(&conn, &rag_rat_db::hooks::MigrationHooks::noop()).unwrap();
    seed_building_generation(&conn);
    conn.busy_timeout(Duration::ZERO).unwrap();
    let writer = Connection::open_with_flags(&path, flags).unwrap();
    writer.busy_timeout(Duration::ZERO).unwrap();
    let before = read_build_checkpoint(&writer);
    let lock = writer.unchecked_transaction().unwrap();
    writer
        .execute("UPDATE clone_graph_generations SET cursor_symbol_id = cursor_symbol_id", [])
        .unwrap();
    let (mut batch, mut postings) = pending_batch();
    let mut waits = 0;
    trace_flush_attempts(&conn);
    let result = flush(&conn, &mut batch, &mut postings).flush(BatchRetry {
        backoff: BatchRetry::default().backoff,
        sleep: |_| {
            waits += 1;
            assert!(waits <= 2, "LOCKED flush exceeded two permitted waits");
        },
    });
    drop(lock);
    let error = result.unwrap_err();
    assert!(
        matches!(error.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::DatabaseLocked),
        "{error:?}"
    );
    assert_eq!(waits, 0, "SQLITE_LOCKED is not SQLITE_BUSY: non-BUSY must surface immediately");
    assert_eq!(FLUSH_ATTEMPTS.get(), 1, "SQLITE_LOCKED must run exactly one attempt");
    assert_eq!(read_build_checkpoint(&writer), before);
    assert_eq!((batch.len(), postings.len()), (1, 1));
    assert!(conn.is_autocommit());
    assert!(!is_sqlite_busy(&error.context("flushing batch checkpoint")));
    for extended_code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_BUSY_RECOVERY,
        rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
        rusqlite::ffi::SQLITE_BUSY_TIMEOUT,
    ] {
        let error = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(extended_code),
            None,
        ))
        .context("flushing batch checkpoint");
        assert!(is_sqlite_busy(&error), "wrapped SQLite code {extended_code} must retry");
    }

    // Contrast the terminal LOCKED case with a retryable writer collision, so removing
    // retries cannot satisfy this negative case merely by rejecting all contention.
    let fixture = Fixture::new();
    let mut lock = Some(fixture.lock_writer());
    let mut busy_waits = 0;
    let result = flush(&fixture.conn, &mut batch, &mut postings).flush(BatchRetry {
        backoff: BatchRetry::default().backoff,
        sleep: |_| {
            busy_waits += 1;
            assert!(busy_waits <= 1, "released-writer flush exceeded one permitted wait");
            lock.take().unwrap().rollback().unwrap();
        },
    });
    drop(lock);
    assert!(result.is_ok(), "SQLITE_BUSY must still retry: {result:?}");
    assert_eq!(result.unwrap(), 1);
    assert_eq!(BUSY_ATTEMPTS.get(), 1);
    let after = read_build_checkpoint(&fixture.writer);
    assert_eq!(
        (after.edges, after.postings, after.generation.1, after.generation.2),
        (1, 2, 42, 1)
    );
    assert!(batch.is_empty() && postings.is_empty());
    assert_retry_exhausts_within_wait_budget(&fixture);
}
