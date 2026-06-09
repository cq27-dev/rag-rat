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

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use sha2::{Digest, Sha256};

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
            Err(TryLockError::Error(err)) => {
                Err(anyhow::Error::from(err).context(format!("try-locking {}", path.display())))
            },
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
/// `canonicalize` resolves symlink aliases (the common way one checkout is reached via two paths) to
/// one key. We deliberately do **not** case-fold: folding would, on a case-sensitive volume,
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
    let name = format!("{}.sock", worktree_hash(worktree_root));
    let preferred = base_dir.join("sockets").join(&name);
    if preferred.as_os_str().len() <= MAX_SOCKET_PATH_LEN {
        return preferred;
    }
    let runtime_base =
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    runtime_base.join("rag-rat").join(name)
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

    #[test]
    fn hook_socket_path_falls_back_when_base_path_is_too_long() {
        // sun_path is ~108 bytes; a deeply nested base dir must divert to a short runtime dir.
        let mut long_base = temp_dir();
        for _ in 0..12 {
            long_base.push("very-long-directory-segment");
        }
        let root = temp_dir();
        let socket = hook_socket_path(&long_base, &root);
        assert!(
            socket.as_os_str().len() <= MAX_SOCKET_PATH_LEN,
            "fallback path still too long: {}",
            socket.display()
        );
    }
}
