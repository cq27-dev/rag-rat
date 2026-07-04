//! Cross-platform file locks for the index file watcher, using std's native `File::{lock,
//! try_lock, unlock}` (stable since Rust 1.89 — `flock` on Unix, `LockFileEx` on Windows, no
//! external crate). Two distinct locks coordinate writers without an HTTP daemon:
//!
//! - the **per-worktree election lock** (one watcher per worktree), keyed by the canonicalized
//!   worktree root and living under the git common dir;
//! - the **per-DB, per-repo write-serialization lock** (held by the watcher, the git hooks, and
//!   manual `index`), so exactly one writer touches a repo's slice of the shared index at a time.
//!
//! Locks release when the file handle drops (the OS also releases on process death), so there is no
//! stale-pidfile cleanup. Caveat: file locks are unreliable on NFS and WSL2 `drvfs`/`9p` mounts.
//!
//! # Canonical lock order (A6, batch-5 P2)
//!
//! When a flow must hold MORE THAN ONE per-repo write lock (today: the LocalOnly→Portable identity
//! upgrade, and a writer whose resolved repo id turns out to differ from the one it locked at
//! entry), the CANONICAL TOTAL ORDER is LEXICOGRAPHIC on the sanitized discriminator
//! ([`canonical_lock_order`]) — supersedes the earlier role-based
//! "incoming-then-outgoing" argument, which broke the moment a second multi-lock path appeared
//! with the opposite roles. Multi-lock acquirers sort the ids they need and acquire in that order
//! wherever they start from a clean slate; an acquisition that would VIOLATE the order (the
//! earlier-sorting lock requested while a later-sorting one is already held — unavoidable when
//! the entry lock was taken identity-blind before the second id was knowable) MUST be BOUNDED
//! (`acquire_timeout`, never `acquire_blocking`): the bounded out-of-order edge is what keeps the
//! pre-held-entry-lock topology deadlock-free — a wait cycle needs two hold-and-wait edges, purely
//! in-order unbounded edges cannot form a cycle under a total order, and any cycle containing a
//! bounded edge self-breaks within its timeout (one side surfaces a retryable error).

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

impl Drop for FileLock {
    fn drop(&mut self) {
        // Release the flock EXPLICITLY, not by relying on the fd-close that follows. flock
        // ownership belongs to the open-file-description, so a child forked while this lock is held
        // (any `std::process::Command` on another thread — CLOEXEC closes fds only at exec, and the
        // fork→exec window keeps the OFD alive) inherits a reference that keeps the close from
        // releasing the lock until the child execs (#409). `unlock()` (flock `LOCK_UN`) drops the
        // lock now, regardless of surviving OFD references in pre-exec children. Best-effort: the
        // fd still closes right after, so ignore the error.
        let _ = self._file.unlock();
    }
}

/// A short, filesystem-safe discriminator for a repo's per-DB lock files (A6): the first 12 ASCII
/// alphanumerics of `repo_id`. Stable per repo (a portable id is a hex root-commit hash; a `local:`
/// id and the root-hash fallback are hex too) and distinct enough that different repos in one
/// shared global DB never share a lock file — 12 chars ≈ 48 bits, collision-negligible for the
/// handful of repos on a machine. Empty / all-punctuation ids degrade to a fixed stem rather than a
/// bare `rag-rat-write-.lock`.
fn lock_discriminator(repo_id: &str) -> String {
    let disc: String = repo_id.chars().filter(char::is_ascii_alphanumeric).take(12).collect();
    if disc.is_empty() { "repo".to_string() } else { disc }
}

/// Per-DB, PER-REPO write-serialization lock path: next to the index database, keyed by `repo_id`
/// (A6). On a shared global DB this keeps two writers to DIFFERENT repos from serializing on one
/// flock — a full rebuild of repo A must not block a memory write to repo B beyond the busy-timeout
/// slice (spec §3.3) — while writers to the SAME repo (across its worktrees, which share a repo_id)
/// still contend. The concurrent access to the SQLite file itself is serialized by WAL, not this
/// lock. Resolve `repo_id` before opening via [`write_lock_repo_id`].
pub fn write_lock_path(database: &Path, repo_id: &str) -> PathBuf {
    database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("rag-rat-write-{}.lock", lock_discriminator(repo_id)))
}

/// The GLOBAL schema-migration lock path, beside the DB (A6): ONE per database file, taken only by
/// the open-time auto-migrate ([`crate::index`] lifecycle). A schema migration rewrites the SHARED
/// migration ladder — every repo's tables — so it must serialize across ALL repos, unlike the
/// per-repo [`write_lock_path`]. Keeping it separate means a repo's ordinary write is neither
/// blocked by nor blocks an unrelated repo except during the brief migration itself.
pub fn schema_lock_path(database: &Path) -> PathBuf {
    database.parent().unwrap_or_else(|| Path::new(".")).join("rag-rat-schema.lock")
}

/// The GLOBAL repo-registry lock path, beside the DB (A7): ONE per database file, taken by
/// `register_repo` for its whole read-decide-write sequence. Registration reads the registered-repo
/// set, DECIDES (idempotent / upgrade / refuse / fresh), then writes `repos`/`repo_roots` — two
/// concurrent first-registrations on the shared global DB would otherwise interleave between the
/// read and the write (a `SQLITE_BUSY_SNAPSHOT` on the deferred upgrade, or a `repos`-PK constraint
/// on the same-id race). Per-repo write locks cannot serialize this: the writers hold DIFFERENT
/// repo ids by construction. GLOBAL-LOCK ORDERING RULE (shared with [`schema_lock_path`]): a global
/// lock is acquired while per-repo entry locks may already be held (per-repo → global), and any
/// per-repo lock taken while a global lock is held (the upgrade path) must be BOUNDED — bounded
/// edges self-break any cross-type cycle within their timeout, exactly like the canonical-order
/// rule's out-of-order edges.
pub fn registry_lock_path(database: &Path) -> PathBuf {
    database.parent().unwrap_or_else(|| Path::new(".")).join("rag-rat-registry.lock")
}

/// Per-DB, PER-REPO maintenance coordination lock, held by the running `rag-rat maintenance`
/// command for its whole pass so the multiple git hooks a single amend/merge/rebase fires coalesce
/// into one pass instead of each running a full discover (#267). Keyed by `repo_id` (A6) like
/// [`write_lock_path`], so a maintenance pass on one repo never coalesces (or blocks) an unrelated
/// repo's. Separate from the write lock so it only coordinates CLI maintenance invocations — the
/// pass itself still takes the write lock internally.
pub fn maintenance_lock_path(database: &Path, repo_id: &str) -> PathBuf {
    database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("rag-rat-maintenance-{}.lock", lock_discriminator(repo_id)))
}

/// Marker a coalesced `maintenance` trigger sets to ask the in-flight runner to run one more pass
/// after the current one, so a change that arrived mid-pass is still covered (#267). Per-repo (A6),
/// pairing with the per-repo [`maintenance_lock_path`].
pub fn maintenance_pending_path(database: &Path, repo_id: &str) -> PathBuf {
    database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("rag-rat-maintenance-{}.pending", lock_discriminator(repo_id)))
}

/// Order two repo ids by the CANONICAL LOCK ORDER (see the module doc): lexicographic on the
/// sanitized discriminator, i.e. the order of the lock FILE NAMES themselves. Multi-lock
/// acquirers sort with this before acquiring. Ties (same discriminator — same repo) are the
/// reentrant case, not an ordering question.
pub fn canonical_lock_order<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if lock_discriminator(a) <= lock_discriminator(b) { (a, b) } else { (b, a) }
}

/// Whether THIS THREAD currently holds the per-repo write lock for `(database, repo_id)` — the
/// reentrancy registry probe, exposed so identity-resolution code can tell "my held entry lock
/// already covers the resolved repo" from the batch-5 fence gap (entry lock keyed by a stale
/// derived id).
pub fn thread_holds_write_lock(database: &Path, repo_id: &str) -> bool {
    write_lock_held_on_this_thread(&write_lock_path(database, repo_id))
}

/// Whether THIS THREAD holds ANY per-repo write lock for `database` (any `rag-rat-write-*.lock`
/// sibling of the DB file; the global schema lock and maintenance locks do not count). `true` for
/// a LOCKED writer (CLI/watcher entry), `false` for the deliberately lockless openers (MCP reads,
/// heals) — the discriminating probe for the batch-5 rule "a writer's held lock must match the
/// repo id it writes under": only a flow that IS lock-disciplined must extend its coverage when
/// the resolved id differs; a lockless flow stays lockless.
pub fn thread_holds_any_repo_write_lock(database: &Path) -> bool {
    let dir = database.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    HELD_WRITE_LOCKS.with_borrow(|held| {
        held.iter().any(|(path, &depth)| {
            depth > 0
                && path.parent() == Some(dir.as_path())
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rag-rat-write-"))
        })
    })
}

/// The repo discriminator for a config's per-DB write / maintenance locks (A6). Resolution CANNOT
/// open the DB (this lock guards the open itself), so it derives the identity straight from git via
/// [`resolve_repo_identity`](crate::repo_identity::resolve_repo_identity), honoring an `[index]
/// repo_id` override — worktrees of one repo resolve to the same id and therefore share a lock. A
/// non-git / unresolvable root (a bare temp dir, a rejected pin) falls back to a stable hash of the
/// canonicalized root; a per-repo DB there holds exactly one repo, so any stable value serializes
/// its writers correctly.
pub fn write_lock_repo_id(config: &Config) -> String {
    crate::repo_identity::resolve_repo_identity(&config.root, config.repo_id_override.as_deref())
        .map(|identity| identity.repo_id)
        .unwrap_or_else(|_| format!("root{}", worktree_hash(&config.root)))
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
    /// Blocks until the PER-REPO write lock is acquired (returns immediately if this thread already
    /// holds it). Use only for non-interactive writers; interactive / hook callers use
    /// [`WriteLock::acquire_timeout`]. `repo_id` keys the lock file (A6) — resolve it via
    /// [`write_lock_repo_id`].
    pub fn acquire_blocking(database: &Path, repo_id: &str) -> anyhow::Result<WriteLock> {
        Self::acquire_path_blocking(write_lock_path(database, repo_id))
    }

    /// Polls until the PER-REPO write lock is acquired or `timeout` elapses (returns immediately if
    /// this thread already holds it); `Ok(None)` on timeout. `repo_id` keys the lock file (A6).
    pub fn acquire_timeout(
        database: &Path,
        repo_id: &str,
        timeout: Duration,
    ) -> anyhow::Result<Option<WriteLock>> {
        Self::acquire_path_timeout(write_lock_path(database, repo_id), timeout)
    }

    /// Polls until the GLOBAL schema-migration lock ([`schema_lock_path`]) is acquired or `timeout`
    /// elapses; `Ok(None)` on timeout. Taken ONLY by the open-time auto-migrate (A6): a schema
    /// migration rewrites the shared ladder, so it serializes across ALL repos, unlike the per-repo
    /// write lock. Reentrant on the holding thread like the per-repo lock (a command already
    /// holding it that re-opens under it does not self-deadlock).
    pub fn acquire_schema_timeout(
        database: &Path,
        timeout: Duration,
    ) -> anyhow::Result<Option<WriteLock>> {
        Self::acquire_path_timeout(schema_lock_path(database), timeout)
    }

    /// Polls until the GLOBAL repo-registry lock ([`registry_lock_path`]) is acquired or `timeout`
    /// elapses; `Ok(None)` on timeout. Taken by `register_repo` for its whole read-decide-write
    /// sequence (A7) — registration decisions span all repos, so per-repo locks cannot serialize
    /// them. Reentrant on the holding thread like the other write locks (a `consolidate` that
    /// pre-registers, then imports, may re-enter registration idempotently).
    pub fn acquire_registry_timeout(
        database: &Path,
        timeout: Duration,
    ) -> anyhow::Result<Option<WriteLock>> {
        Self::acquire_path_timeout(registry_lock_path(database), timeout)
    }

    fn acquire_path_blocking(lock_path: PathBuf) -> anyhow::Result<WriteLock> {
        if write_lock_held_on_this_thread(&lock_path) {
            register_write_lock(&lock_path);
            return Ok(WriteLock { _inner: None, lock_path });
        }
        let inner = FileLock::acquire_blocking(&lock_path)?;
        register_write_lock(&lock_path);
        Ok(WriteLock { _inner: Some(inner), lock_path })
    }

    fn acquire_path_timeout(
        lock_path: PathBuf,
        timeout: Duration,
    ) -> anyhow::Result<Option<WriteLock>> {
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

    /// #409: flock ownership belongs to the open-file-description, not the fd. A child forked
    /// while the lock is held (any sibling test shelling out to `git` in another libtest thread)
    /// inherits the OFD, so releasing the lock by fd-close alone leaves it held until the child
    /// execs (CLOEXEC closes fds only at exec — the fork→exec window keeps the OFD alive). The
    /// explicit `File::unlock()` in `Drop for FileLock` releases immediately regardless of
    /// surviving OFD references. Here the child never execs — it just sleeps — so ONLY the explicit
    /// unlock can free the lock; close alone would keep it held for the child's whole lifetime.
    #[cfg(unix)]
    #[test]
    fn drop_releases_flock_immediately_despite_a_fork_inherited_fd() {
        let dir = temp_dir();
        let path = dir.join("fork.lock");

        let held = FileLock::try_acquire(&path).unwrap().expect("first acquire should succeed");

        // Existing behavior still holds: while genuinely held in-process (no fork in play yet), a
        // second try_acquire is blocked — so a later `Some` is a real release, not always-Some.
        assert!(
            FileLock::try_acquire(&path).unwrap().is_none(),
            "a second acquire must fail while the lock is genuinely held"
        );

        // Fork a child that inherits the locked OFD and keeps it alive (no exec) for ~300ms.
        // SAFETY: post-fork in a possibly-multithreaded test binary the child may run only
        // async-signal-safe libc calls and must not allocate or run Rust runtime teardown; it does
        // neither — just `usleep` then `_exit` (no unwind, no atexit).
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe {
                libc::usleep(300_000);
                libc::_exit(0);
            }
        }

        // Parent: drop our handle. With the explicit unlock this frees the flock now; relying on
        // fd-close alone would leave it held while the child holds the inherited OFD open.
        drop(held);

        let reacquired = FileLock::try_acquire(&path).unwrap();

        // Reap the child before asserting so a failure path still doesn't leak a zombie.
        let mut status: libc::c_int = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };

        assert!(
            reacquired.is_some(),
            "drop must release the flock immediately despite the fork-inherited fd (#409)"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_lock_is_reentrant_within_a_thread_but_not_across_threads() {
        // #226: a CLI/watcher write command holds the write lock and opens UNDER it (the open-time
        // schema migrate re-acquires). Reentrant on the SAME thread → no self-deadlock; a different
        // thread is a genuine second writer → must still contend on the real OS lock.
        let dir = temp_dir();
        let db = dir.join("index.sqlite");
        let repo = "repoaaaa0000";

        let outer = WriteLock::acquire_blocking(&db, repo).unwrap();
        // Nested acquire on the same thread re-enters immediately (a raw lock would self-deadlock).
        let inner = WriteLock::acquire_timeout(&db, repo, Duration::from_millis(50)).unwrap();
        assert!(inner.is_some(), "nested same-thread acquire must re-enter, not block");
        drop(inner);

        // While `outer` is still held, a DIFFERENT thread must not be able to acquire it.
        let db_other = db.clone();
        let got = std::thread::spawn(move || {
            WriteLock::acquire_timeout(&db_other, repo, Duration::from_millis(100))
                .unwrap()
                .is_some()
        })
        .join()
        .unwrap();
        assert!(!got, "a second thread must contend on the real OS lock while the first holds it");

        // A6: a DIFFERENT repo's write lock is an INDEPENDENT file, so it acquires even while this
        // repo's lock is held — the whole point of per-repo locks (a rebuild of one repo must not
        // block a write to another in a shared DB).
        let db_sibling = db.clone();
        let sibling_got = std::thread::spawn(move || {
            WriteLock::acquire_timeout(&db_sibling, "repobbbb1111", Duration::from_millis(100))
                .unwrap()
                .is_some()
        })
        .join()
        .unwrap();
        assert!(sibling_got, "a different repo's write lock must not be blocked by this repo's");

        drop(outer);
        // Fully released once the outermost guard drops: a fresh (cross-thread) acquire succeeds.
        let db_after = db.clone();
        let reacquired = std::thread::spawn(move || {
            WriteLock::acquire_timeout(&db_after, repo, Duration::from_millis(100))
                .unwrap()
                .is_some()
        })
        .join()
        .unwrap();
        assert!(reacquired, "the lock is free after the outermost guard drops");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn canonical_lock_order_is_total_and_lexicographic_on_the_discriminator() {
        // A portable (hex-leading) id sorts before a `local:`-derived one (hex digits < 'l' after
        // sanitization), whichever way the pair is passed — the total order every multi-lock
        // acquirer sorts by (see the module doc).
        assert_eq!(
            canonical_lock_order("0abc123root", "local:deadbeef"),
            ("0abc123root", "local:deadbeef")
        );
        assert_eq!(
            canonical_lock_order("local:deadbeef", "0abc123root"),
            ("0abc123root", "local:deadbeef")
        );
        // Equal discriminators (the same repo) are the reentrant case, not an ordering question;
        // the order is merely stable.
        assert_eq!(canonical_lock_order("aaaa", "aaaa"), ("aaaa", "aaaa"));
    }

    #[test]
    fn lock_paths_are_per_repo_but_the_schema_lock_is_global() {
        // A6: the write / maintenance locks are keyed by repo_id (different repos → different
        // files), while the schema-migration lock is one file per DB regardless of repo.
        let db = Path::new("/repo/.rag-rat/index.sqlite");
        assert_ne!(write_lock_path(db, "aaaa11112222"), write_lock_path(db, "bbbb33334444"));
        assert_eq!(write_lock_path(db, "aaaa11112222"), write_lock_path(db, "aaaa11112222"));
        assert_ne!(
            maintenance_lock_path(db, "aaaa11112222"),
            maintenance_lock_path(db, "bbbb33334444")
        );
        // The schema lock ignores the repo entirely (one migration serializer per DB file).
        assert_eq!(schema_lock_path(db), schema_lock_path(db));
        assert!(schema_lock_path(db).to_string_lossy().ends_with("rag-rat-schema.lock"));
        // A `local:`-prefixed id sanitizes to alphanumerics (the `:` is dropped), still per-repo.
        assert_eq!(write_lock_path(db, "local:abcdef01"), write_lock_path(db, "local:abcdef01"));
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
        // Short, fixed paths (they need not exist — `hook_socket_path` only builds the string) so
        // the preferred `<base>/sockets/<hash>.sock` stays within MAX_SOCKET_PATH_LEN on
        // every platform. The real `temp_dir()` helper roots under `std::env::temp_dir()`,
        // which on macOS is a long `/var/folders/…` path that busts the budget and falls
        // back to the XDG candidate (parent `rag-rat`, not `sockets`) — the over-budget
        // fallback is covered separately via `long_base_dir`.
        let base = Path::new("/r");
        let root = Path::new("/repo");
        let socket = hook_socket_path(base, root);
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
