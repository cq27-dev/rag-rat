//! Cross-platform file locks for the index file watcher, using std's native `File::{lock,
//! try_lock, unlock}` (stable since Rust 1.89 — `flock` on Unix, `LockFileEx` on Windows, no
//! external crate). Two distinct locks coordinate writers without an HTTP daemon:
//!
//! - the **per-worktree election lock** (one watcher per worktree), keyed by the canonicalized
//!   worktree root and living under the git common dir;
//! - the **per-DB write-serialization lock** (held by the watcher, the git hooks, and manual
//!   `index`), so exactly one writer touches the shared index at a time.
//!
//! Locks release when the file handle drops (the OS also releases on process death), so there is no
//! stale-pidfile cleanup. Caveat: file locks are unreliable on NFS and WSL2 `drvfs`/`9p` mounts.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use sha2::{Digest, Sha256};

use crate::config::Config;

/// A held exclusive file lock. Released on drop.
#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

impl FileLock {
    fn open(path: &Path) -> anyhow::Result<File> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating lock dir {}", parent.display()))?;
        }
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening lock file {}", path.display()))
    }

    /// Non-blocking. `Ok(Some)` if acquired now, `Ok(None)` if another holder has it.
    pub fn try_acquire(path: &Path) -> anyhow::Result<Option<FileLock>> {
        let file = Self::open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(FileLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) =>
                Err(anyhow::Error::from(err).context(format!("try-locking {}", path.display()))),
        }
    }

    /// Blocks until acquired. Use only watcher-to-watcher; interactive callers use
    /// [`FileLock::acquire_timeout`] so a hung holder can't hang `git checkout`.
    pub fn acquire_blocking(path: &Path) -> anyhow::Result<FileLock> {
        let file = Self::open(path)?;
        file.lock().with_context(|| format!("locking {}", path.display()))?;
        Ok(FileLock { _file: file })
    }

    /// Polls until acquired or `timeout` elapses; `Ok(None)` on timeout (caller should warn-skip).
    pub fn acquire_timeout(path: &Path, timeout: Duration) -> anyhow::Result<Option<FileLock>> {
        let deadline = Instant::now() + timeout;
        let poll = Duration::from_millis(50).min(timeout.max(Duration::from_millis(1)));
        loop {
            if let Some(lock) = Self::try_acquire(path)? {
                return Ok(Some(lock));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(poll);
        }
    }
}

/// Per-DB write-serialization lock path: next to the index database (under the git common dir for a
/// shared DB, or `<root>/.rag-rat/` for a single worktree — both excluded from the watch tree).
pub fn write_lock_path(database: &Path) -> PathBuf {
    database.parent().unwrap_or_else(|| Path::new(".")).join("rag-rat-write.lock")
}

thread_local! {
    /// Reentrancy registry for the per-DB write lock: lock-file path → nesting depth on THIS thread.
    /// Thread-local because the only legitimate nested acquire is an `open()`-time schema migrate
    /// running synchronously inside a command / watcher pass that already holds the lock on the SAME
    /// thread (#226). A different thread wanting the write lock is a genuine second writer and must
    /// contend on the real OS flock, so it deliberately does not see this thread's hold.
    static HELD_WRITE_LOCKS: RefCell<BTreeMap<PathBuf, usize>> = const { RefCell::new(BTreeMap::new()) };
}

fn write_lock_held_on_this_thread(lock_path: &Path) -> bool {
    HELD_WRITE_LOCKS.with_borrow(|held| held.get(lock_path).is_some_and(|&depth| depth > 0))
}

fn register_write_lock(lock_path: &Path) {
    HELD_WRITE_LOCKS.with_borrow_mut(|held| *held.entry(lock_path.to_path_buf()).or_insert(0) += 1);
}

fn release_write_lock(lock_path: &Path) {
    HELD_WRITE_LOCKS.with_borrow_mut(|held| {
        if let Some(depth) = held.get_mut(lock_path) {
            *depth -= 1;
            if *depth == 0 {
                held.remove(lock_path);
            }
        }
    });
}

/// The per-DB write-serialization lock, made REENTRANT within a thread (unlike the raw
/// [`FileLock`]).
///
/// A CLI write command (`index` / `maintenance` / `oracle run`) and a watcher maintenance pass hold
/// this lock for their whole duration, and the `open()` they run inside it may itself need to
/// migrate an `Older` schema under the same lock. With a raw file lock that is a SELF-DEADLOCK: the
/// same process re-`flock`s the file on a second fd, which blocks (flock is per-open-file-
/// description), times out after 30s, and leaves the schema unmigrated (#226). `WriteLock` records
/// the hold in a thread-local registry, so a nested acquire on the same thread is reentrant — it
/// skips the OS lock and just bumps a depth count; only the outermost guard takes and releases the
/// real `flock`. A second THREAD still contends on the OS lock, so cross-thread it behaves like a
/// normal exclusive lock.
#[derive(Debug)]
pub struct WriteLock {
    /// `None` for a reentrant inner acquire (this thread already held the lock, so no OS lock was
    /// taken — dropping it must only decrement the depth, never unlock the file).
    _inner: Option<FileLock>,
    lock_path: PathBuf,
}

impl WriteLock {
    /// Blocks until acquired (returns immediately if this thread already holds it). Use only for
    /// non-interactive writers; interactive / hook callers use [`WriteLock::acquire_timeout`].
    pub fn acquire_blocking(database: &Path) -> anyhow::Result<WriteLock> {
        let lock_path = write_lock_path(database);
        if write_lock_held_on_this_thread(&lock_path) {
            register_write_lock(&lock_path);
            return Ok(WriteLock { _inner: None, lock_path });
        }
        let inner = FileLock::acquire_blocking(&lock_path)?;
        register_write_lock(&lock_path);
        Ok(WriteLock { _inner: Some(inner), lock_path })
    }

    /// Polls until acquired or `timeout` elapses (returns immediately if this thread already holds
    /// it); `Ok(None)` on timeout.
    pub fn acquire_timeout(
        database: &Path,
        timeout: Duration,
    ) -> anyhow::Result<Option<WriteLock>> {
        let lock_path = write_lock_path(database);
        if write_lock_held_on_this_thread(&lock_path) {
            register_write_lock(&lock_path);
            return Ok(Some(WriteLock { _inner: None, lock_path }));
        }
        match FileLock::acquire_timeout(&lock_path, timeout)? {
            Some(inner) => {
                register_write_lock(&lock_path);
                Ok(Some(WriteLock { _inner: Some(inner), lock_path }))
            },
            None => Ok(None),
        }
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        // Decrement first; `_inner` then drops AFTER this body, unlocking the OS flock for the
        // outermost guard. A reentrant guard has `_inner: None`, so its drop only decrements.
        release_write_lock(&self.lock_path);
    }
}

/// `sun_path` budget for Unix domain sockets (108 bytes on Linux, 104 on macOS) with headroom.
pub const MAX_SOCKET_PATH_LEN: usize = 100;

/// Stable per-worktree key: sha256 of the canonicalized root (see `election_lock_path` doc
/// comment for why canonicalize-but-not-case-fold).
fn worktree_hash(worktree_root: &Path) -> String {
    let canonical = worktree_root.canonicalize().unwrap_or_else(|_| worktree_root.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let mut hash = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(hash, "{byte:02x}");
    }
    hash
}

/// Per-worktree election lock path, keyed by a hash of the **canonicalized** worktree root —
/// `canonicalize` resolves symlink aliases (the common way one checkout is reached via two paths)
/// to one key. We deliberately do **not** case-fold: folding would, on a case-sensitive volume,
/// collapse two genuinely-distinct worktrees into one key and leave one permanently un-elected
/// (silent staleness — the exact failure this design exists to prevent). The remaining edge — the
/// same checkout reached via differently-cased paths on a case-insensitive FS — merely elects two
/// watchers, which the write lock makes harmless. `base_dir` is the index DB's directory (the
/// shared location across a repo's worktrees), so all election locks sit under `<base_dir>/locks/`.
pub fn election_lock_path(base_dir: &Path, worktree_root: &Path) -> PathBuf {
    base_dir.join("locks").join(format!("{}.lock", worktree_hash(worktree_root)))
}

/// Election lock for the grep-augment hook socket: one listener per worktree, separate from the
/// watcher election so core never calls back into the MCP crate and either process may win each.
pub fn socket_lock_path(base_dir: &Path, worktree_root: &Path) -> PathBuf {
    base_dir.join("locks").join(format!("{}.socket.lock", worktree_hash(worktree_root)))
}

/// Where the elected listener binds. Prefers a `sockets/` sibling of `locks/` under the shared
/// DB dir; diverts to `$XDG_RUNTIME_DIR/rag-rat/` then the OS temp dir when the result would
/// exceed the `sun_path` budget. Hook clients compute the same path independently, so this must
/// stay deterministic for a given (base_dir, worktree_root) and environment.
pub fn hook_socket_path(base_dir: &Path, worktree_root: &Path) -> PathBuf {
    let runtime_base =
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    socket_path_with_runtime_base(base_dir, worktree_root, &runtime_base)
}

/// Single source of truth for the hook socket path given a `Config`. Shared by the MCP listener
/// and the CLI client so the two cannot diverge.
pub fn hook_socket_path_for(config: &Config) -> PathBuf {
    let base =
        config.database.parent().map(Path::to_path_buf).unwrap_or_else(|| config.root.clone());
    hook_socket_path(&base, &config.root)
}

/// Single source of truth for the hook socket election-lock path given a `Config`. Shared by the
/// MCP listener and the CLI client so the two cannot diverge.
pub fn hook_socket_lock_path_for(config: &Config) -> PathBuf {
    let base =
        config.database.parent().map(Path::to_path_buf).unwrap_or_else(|| config.root.clone());
    socket_lock_path(&base, &config.root)
}

/// Inner implementation: builds the candidate path cascade with an explicit `runtime_base` so the
/// fallback logic can be unit-tested without touching the process environment.
///
/// Priority:
/// 1. `<base_dir>/sockets/<hash>.sock` — within budget?  Use it.
/// 2. `<runtime_base>/rag-rat/<hash>.sock` — within budget?  Use it.
/// 3. `<temp_dir>/rag-rat/<hash>.sock` — best effort; callers fail open if still over budget.
fn socket_path_with_runtime_base(
    base_dir: &Path,
    worktree_root: &Path,
    runtime_base: &Path,
) -> PathBuf {
    let name = format!("{}.sock", worktree_hash(worktree_root));
    let preferred = base_dir.join("sockets").join(&name);
    if preferred.as_os_str().len() <= MAX_SOCKET_PATH_LEN {
        return preferred;
    }
    let xdg_candidate = runtime_base.join("rag-rat").join(&name);
    if xdg_candidate.as_os_str().len() <= MAX_SOCKET_PATH_LEN {
        return xdg_candidate;
    }
    // Both preferred and XDG are over budget — fall through to the OS temp dir.
    // If even this is over budget there is nothing better; callers fail open.
    std::env::temp_dir().join("rag-rat").join(name)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static LOCK_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let id = LOCK_TEMP.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ragrat-lock-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn exclusive_lock_blocks_second_holder_and_releases_on_drop() {
        let dir = temp_dir();
        let path = dir.join("a.lock");

        let first = FileLock::try_acquire(&path).unwrap();
        assert!(first.is_some(), "first acquire should succeed");

        let second = FileLock::try_acquire(&path).unwrap();
        assert!(second.is_none(), "second acquire must fail while held");

        // A different path is independent (cross-project isolation).
        let other = FileLock::try_acquire(&dir.join("b.lock")).unwrap();
        assert!(other.is_some(), "a different lock path should acquire");

        drop(first);
        let reacquired = FileLock::try_acquire(&path).unwrap();
        assert!(reacquired.is_some(), "should acquire after the holder drops");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_lock_is_reentrant_within_a_thread_but_not_across_threads() {
        // #226: a CLI/watcher write command holds the write lock and opens UNDER it (the open-time
        // schema migrate re-acquires). Reentrant on the SAME thread → no self-deadlock; a different
        // thread is a genuine second writer → must still contend on the real OS lock.
        let dir = temp_dir();
        let db = dir.join("index.sqlite");

        let outer = WriteLock::acquire_blocking(&db).unwrap();
        // Nested acquire on the same thread re-enters immediately (a raw lock would self-deadlock).
        let inner = WriteLock::acquire_timeout(&db, Duration::from_millis(50)).unwrap();
        assert!(inner.is_some(), "nested same-thread acquire must re-enter, not block");
        drop(inner);

        // While `outer` is still held, a DIFFERENT thread must not be able to acquire it.
        let db_other = db.clone();
        let got = std::thread::spawn(move || {
            WriteLock::acquire_timeout(&db_other, Duration::from_millis(100)).unwrap().is_some()
        })
        .join()
        .unwrap();
        assert!(!got, "a second thread must contend on the real OS lock while the first holds it");

        drop(outer);
        // Fully released once the outermost guard drops: a fresh (cross-thread) acquire succeeds.
        let db_after = db.clone();
        let reacquired = std::thread::spawn(move || {
            WriteLock::acquire_timeout(&db_after, Duration::from_millis(100)).unwrap().is_some()
        })
        .join()
        .unwrap();
        assert!(reacquired, "the lock is free after the outermost guard drops");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn election_path_is_stable_per_root_and_distinct_across_roots() {
        let base = Path::new("/repo/.git/rag-rat");
        let a1 = election_lock_path(base, Path::new("/repo"));
        let a2 = election_lock_path(base, Path::new("/repo"));
        let b = election_lock_path(base, Path::new("/repo-wt"));
        assert_eq!(a1, a2, "same worktree root → same lock");
        assert_ne!(a1, b, "different worktree roots → different locks");
        assert!(a1.starts_with(base.join("locks")));
    }

    #[test]
    fn socket_lock_path_is_distinct_from_election_lock_path() {
        let base = temp_dir();
        let root = temp_dir();
        let election = election_lock_path(&base, &root);
        let socket_lock = socket_lock_path(&base, &root);
        assert_ne!(election, socket_lock);
        assert!(socket_lock.to_string_lossy().ends_with(".socket.lock"));
        // Same worktree key: both live under <base>/locks/ with the same hash stem.
        assert_eq!(election.parent(), socket_lock.parent());
    }

    #[test]
    fn hook_socket_path_lives_under_base_sockets_dir() {
        let base = temp_dir();
        let root = temp_dir();
        let socket = hook_socket_path(&base, &root);
        assert_eq!(socket.parent().unwrap().file_name().unwrap(), "sockets");
        assert!(socket.extension().is_some_and(|ext| ext == "sock"));
    }

    /// Build a base dir long enough that `<base>/sockets/<hash>.sock` exceeds
    /// `MAX_SOCKET_PATH_LEN`.
    fn long_base_dir() -> PathBuf {
        let mut base = temp_dir();
        // Each push appends ~28 bytes; 12 × 28 = 336, well over the 100-byte budget.
        for _ in 0..12 {
            base.push("very-long-directory-segment");
        }
        base
    }

    #[test]
    fn hook_socket_path_falls_back_when_base_path_is_too_long() {
        // When the preferred path is over budget and XDG_RUNTIME_DIR is a short /tmp path, the
        // XDG candidate fits within budget and is returned.
        let long_base = long_base_dir();
        let root = temp_dir();
        // Use a known-short runtime_base so the test is independent of the runner environment.
        let short_runtime_base = std::env::temp_dir(); // e.g. /tmp — always short
        let socket = socket_path_with_runtime_base(&long_base, &root, &short_runtime_base);
        assert!(
            socket.as_os_str().len() <= MAX_SOCKET_PATH_LEN,
            "XDG fallback path still too long: {}",
            socket.display()
        );
        // Should NOT live under the long base dir.
        assert!(!socket.starts_with(&long_base), "expected fallback, got preferred path");
    }

    #[test]
    fn hook_socket_path_falls_back_to_temp_when_xdg_also_too_long() {
        // When both the preferred path and the XDG candidate are over budget, the function falls
        // through to the OS temp dir (best-effort; callers fail open).
        let long_base = long_base_dir();
        let long_runtime_base = long_base_dir();
        let root = temp_dir();
        let socket = socket_path_with_runtime_base(&long_base, &root, &long_runtime_base);
        // Must not be under either long base.
        assert!(!socket.starts_with(&long_base));
        assert!(!socket.starts_with(&long_runtime_base));
        // Should be rooted at the OS temp dir.
        assert!(
            socket.starts_with(std::env::temp_dir()),
            "expected temp-dir fallback, got: {}",
            socket.display()
        );
    }
}
