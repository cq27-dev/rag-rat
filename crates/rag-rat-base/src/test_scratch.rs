//! Shared scratch-directory helper for the workspace's test fixtures.
//!
//! Historically every fixture rolled its own `std::env::temp_dir().join(...)` scratch dir, cleaned
//! only by a `Drop` impl. A panicking test, a killed `nextest` run, or a SIGKILL'd process bypasses
//! `Drop` and strands the directory under the system temp — on the dev box 16k dirs / ~14 GB of
//! tmpfs accumulated before a manual sweep (#726).
//!
//! # Why the system temp, not `target/tmp`
//!
//! The obvious "per-checkout, `cargo clean`-reclaimed" fix — relocate scratch under the build's
//! `target/tmp/` — is UNSOUND for this codebase. Many fixtures `git init` a throwaway repo and rely
//! on it having NO ancestor `.git`; `target/tmp/` is inside the project's own git working tree, so
//! rag-rat's git-context discovery walks up and resolves the *outer* rag-rat repo's identity/commit
//! (observed: 295 `schema_bootstrap_tests` / worktree-scope failures, panicking with the outer
//! repo's HEAD sha). Scratch therefore MUST stay in the system temp, isolated from any repo.
//!
//! So this module fixes the leak the other way the issue proposed — a self-healing sweep:
//!   1. all scratch lives under ONE dedicated, namespaced sub-root of the system temp
//!      ([`scratch_root`], `<temp>/rag-rat-test-scratch-<tag>` — the euid on Unix, the sanitized
//!      username on Windows), so a stranded dir is trivially found and the sweep below can never
//!      touch anything but this helper's own dirs;
//!   2. the FIRST scratch request in each process sweeps sibling scratch dirs older than
//!      [`SWEEP_MAX_AGE`] out of that root — self-healing after a `kill -9`, no external cron.
//!
//! [`ScratchDir`]'s `Drop` cleanup is the fast path; the age-bounded sweep is only the backstop for
//! the cases where `Drop` never runs.
//!
//! # Security: the namespace is per-user and its identity is verified before any sweep
//!
//! The system temp is world-writable and shared between local users. A namespace at a *fixed*,
//! predictable path (`<temp>/rag-rat-test-scratch`) is attackable: before our process starts,
//! another local user can pre-create that path as a **symlink to any directory the test-running
//! user can write**. `create_dir_all` accepts the symlink (its target already exists as a dir), and
//! the startup sweep then traverses the *target*, deleting its subdirectories older than
//! [`SWEEP_MAX_AGE`] — attacker-directed deletion of arbitrary directories (#726 review, skakri).
//!
//! Two defences, both below:
//!   * **per-user namespace** — the root carries a per-user tag (the effective uid on Unix, the
//!     sanitized `%USERNAME%` on Windows: `…-<tag>`), so two users sharing the temp get
//!     structurally distinct roots and a name collision is impossible;
//!   * **verify before sweep** — [`ensure_secure_root`] re-inspects the created namespace with
//!     symlink-aware metadata and panics unless it is a real directory (not a symlink, and no
//!     reparse point on Windows). On Unix it additionally requires ownership by our euid, with mode
//!     `0700`; ownership verification is Unix-only (see the fn). A poisoned namespace fails the
//!     suite loudly; it is never swept.
//!
//! # Why the handed-out paths are deliberately NOT canonical
//!
//! Production's `Config::load` canonicalizes its root, and index/overlay code depends on that. On
//! macOS the system temp is already non-canonical (`/var` → `/private/var`) and on Windows it
//! carries 8.3 names (`RUNNER~1`), so a fixture that keeps its own copy of the scratch path is
//! holding a second spelling of the root production never sees — the mis-scoping that produces is
//! invisible on Linux, where the two spellings used to coincide. [`aliased_root`] hands scratch
//! paths out through a symlink inside the namespace so Linux carries that divergence too, and
//! [`canonical_config_root`] is what a fixture's hand-built `Config` normalizes its root with.
//!
//! Not `#[cfg(test)]`: that gate does not propagate across crates, and fixtures in sibling crates
//! (`rag-rat-core`, `rag-rat-cli`, …) consume this helper. It is not part of the semver-stable API
//! surface. Matches the existing `rag_rat_oracle::test_support` convention.

use std::path::{Component, Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Prefix of the dedicated system-temp sub-directory under which ALL test scratch lives. The real
/// namespace appends a per-user tag ([`scratch_root`] → `rag-rat-test-scratch-<tag>`: the
/// effective uid on Unix, the sanitized `%USERNAME%` on Windows) so users sharing the temp never
/// collide. A dedicated namespace keeps the sweep's blast radius to this helper's own dirs (never
/// the shared temp root) and makes manual cleanup a one-liner:
/// `rm -rf $TMPDIR/rag-rat-test-scratch-$(id -u)`.
pub const SCRATCH_NAMESPACE_PREFIX: &str = "rag-rat-test-scratch";

/// Sweep scratch dirs older than this on first use per process. Two hours is far longer than any
/// single test or `nextest` run (CI caps a wedged test at 60s), so the threshold can never delete a
/// LIVE parallel worker's fresh dir — only genuinely stranded ones from a crashed/killed run.
pub const SWEEP_MAX_AGE: Duration = Duration::from_secs(2 * 60 * 60);

/// Process-wide counter that makes scratch names unique WITHIN a process; the PID makes them unique
/// ACROSS processes (nextest runs each test in its own process).
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Guards the once-per-process startup sweep.
static SWEEP: Once = Once::new();

/// The scratch root shared by every test in every crate: `<system
/// temp>/rag-rat-test-scratch-<tag>` (per-user tag: euid on Unix, sanitized username on Windows).
///
/// The system temp (not `target/tmp`) so `git init` fixtures have no ancestor `.git`; a dedicated,
/// per-user namespace so the sweep can only ever remove this helper's own dirs and two users
/// sharing the temp can never collide.
pub fn scratch_root() -> PathBuf {
    std::env::temp_dir().join(format!("{SCRATCH_NAMESPACE_PREFIX}-{}", namespace_user_tag()))
}

/// The per-user tag that suffixes the scratch namespace (`rag-rat-test-scratch-<tag>`), so two
/// local users sharing the system temp get structurally distinct roots and cannot collide on — or
/// pre-plant — each other's namespace.
///
/// Unix: the effective uid — always present, stable for the process, and the same value the
/// ownership verification in [`ensure_secure_root`] checks against.
#[cfg(unix)]
fn namespace_user_tag() -> String {
    current_euid().to_string()
}

/// Windows: the sanitized `%USERNAME%` (`unknown` when unset) — the same collision-avoidance
/// purpose as the euid, without new dependencies. Every char outside `[A-Za-z0-9_-]` becomes `_`
/// so the tag is always a valid path component.
#[cfg(windows)]
fn namespace_user_tag() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".to_string());
    user.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// This process's effective user id, used to make the scratch namespace per-user and to verify
/// namespace ownership before sweeping. Unix-only: Windows has no `geteuid` (and no portable
/// ownership check) — see [`namespace_user_tag`] and [`ensure_secure_root`] for the Windows story.
#[cfg(unix)]
fn current_euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, has no preconditions, and per POSIX always succeeds.
    // On Linux `uid_t` is `u32` (matching `MetadataExt::uid()`); this binding pins that, so a
    // platform with a differently-sized `uid_t` fails to compile here rather than truncating.
    unsafe { libc::geteuid() }
}

/// Securely create the scratch namespace `root` and verify it is a real directory this process
/// may sweep (euid ownership on Unix; reparse-point rejection on Windows), panicking loudly
/// otherwise. Run before every sweep so a poisoned namespace can never be swept.
///
/// # The attack (#726 review, skakri)
///
/// On a shared system temp, another local user can pre-create the namespace path as a **symlink to
/// any directory the test-running user can write**. A plain `create_dir_all` accepts that symlink
/// (its target already exists as a dir), after which the startup sweep traverses the *target* and
/// removes its subdirectories older than [`SWEEP_MAX_AGE`] — arbitrary-directory deletion driven by
/// an attacker's symlink.
///
/// # The defence
///
/// Create the namespace (with mode `0700` on Unix), then re-inspect it with **symlink-aware**
/// [`std::fs::symlink_metadata`] (which does NOT follow links) and require that it is (1) not a
/// symlink and (2) a directory — on Windows additionally that it carries no reparse point at
/// all — and on Unix (3) that it is owned by this process's effective uid. Any deviation is a
/// poisoned namespace: panic and fail the suite rather than sweep a directory we do not own.
/// Windows ownership (SID/ACL) verification is deliberately NOT done: it would require
/// `windows-sys`/`winapi`, and the per-user namespace plus reparse-point rejection is the
/// Windows story (no new dependencies).
///
/// # TOCTOU boundary (stated honestly)
///
/// This verification defeats the *pre-created* symlink — a link planted before we look is caught by
/// `symlink_metadata`, because we inspect the very path we created and are about to sweep. It does
/// NOT close a racing swap performed by an attacker *between* this check and the sweep: closing
/// that window would require dirfd-relative operations (an `O_NOFOLLOW`/`O_DIRECTORY` `openat` plus
/// `fstatat`/`unlinkat` pinned to the verified inode) so every later op targets the same inode.
/// That is out of scope for test scratch: the per-user namespace already makes a collision
/// structurally irrelevant, and a same-user attacker racing our parent temp gains no privilege. The
/// realistic, cheap attack — plant a symlink and wait — is fully closed.
fn ensure_secure_root(root: &Path) {
    create_namespace(root);

    // Re-inspect with symlink-aware metadata: a pre-planted symlink is caught HERE, before any
    // sweep can traverse (and delete inside) its target.
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(err) => panic!("test scratch: cannot stat namespace {root:?}: {err}"),
    };
    let file_type = meta.file_type();
    assert!(
        !file_type.is_symlink(),
        "test scratch: namespace {root:?} is a symlink — refusing to sweep its target (possible \
         tmp symlink-planting attack; see test_scratch.rs)",
    );
    assert!(
        file_type.is_dir(),
        "test scratch: namespace {root:?} exists but is not a directory (found {file_type:?})",
    );

    verify_namespace_identity(root, &meta);
}

/// Create only the namespace (its parent temp already exists) with 0700 from birth, so a freshly
/// created namespace is never group/other-accessible even for an instant. Unix-only: Windows has
/// no mode bits, so there the create is a plain `create_dir_all` (below).
#[cfg(unix)]
fn create_namespace(root: &Path) {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    if let Err(err) = builder.create(root) {
        panic!("test scratch: failed to create namespace {root:?}: {err}");
    }
}

/// Windows namespace creation: no Unix mode bits exist, so a plain recursive create. Same
/// panic-loud failure style as the Unix path.
#[cfg(windows)]
fn create_namespace(root: &Path) {
    if let Err(err) = std::fs::create_dir_all(root) {
        panic!("test scratch: failed to create namespace {root:?}: {err}");
    }
}

/// Unix identity verification: the namespace must be owned by this process's euid, then tightened
/// to mode `0700`.
#[cfg(unix)]
fn verify_namespace_identity(root: &Path, meta: &std::fs::Metadata) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let owner = meta.uid();
    let euid = current_euid();
    assert_eq!(
        owner, euid,
        "test scratch: namespace {root:?} is owned by uid {owner}, not this process's euid {euid} \
         — refusing to sweep a directory this user does not own",
    );

    // Defence in depth: a namespace left by an older build may have looser permissions. Tighten to
    // 0700 (failing loudly if we cannot) so group/other can never plant siblings inside it.
    if meta.mode() & 0o777 != 0o700
        && let Err(err) = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
    {
        panic!("test scratch: cannot set 0700 on namespace {root:?}: {err}");
    }
}

/// Windows identity verification: reject ANY reparse point on the namespace.
///
/// The `is_symlink` assert above ALREADY refuses directory junctions on this toolchain's std
/// (1.95): `FileType::is_symlink` is `FILE_ATTRIBUTE_REPARSE_POINT && reparse_tag & 0x20000000
/// != 0` (the name-surrogate bit), and junctions (`IO_REPARSE_TAG_MOUNT_POINT = 0xA0000003`)
/// carry that bit (see `library/std/src/sys/fs/windows.rs`, `FileType::new`). That junction
/// coverage is an undocumented, std-version-specific implementation detail, so refuse every
/// reparse point outright via `file_attributes()` — version-independent, and it also covers
/// non-name-surrogate tags (e.g. cloud placeholders) that are likewise redirectors we must not
/// sweep through.
///
/// Ownership verification stays Unix-only: Windows SID/ACL checks would need `windows-sys` /
/// `winapi` (rejected — no new dependencies). The per-user namespace plus this reparse-point
/// rejection is the Windows story.
#[cfg(windows)]
fn verify_namespace_identity(root: &Path, meta: &std::fs::Metadata) {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    assert!(
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "test scratch: namespace {root:?} is a reparse point (symlink/junction/...) — refusing to \
         sweep its target (possible temp redirector attack; see test_scratch.rs)",
    );
}

/// Delete scratch dirs directly under `root` whose mtime is older than [`SWEEP_MAX_AGE`].
/// Race-safe: every filesystem call tolerates a dir a parallel worker removed first (ENOENT and
/// friends are ignored), and the age bound guarantees a fresh dir from a live worker is never
/// touched.
fn sweep_stale(root: &Path) {
    let now = SystemTime::now();
    let Ok(entries) = std::fs::read_dir(root) else {
        return; // root not created yet / vanished — nothing to sweep.
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Defence in depth: never follow a symlink. Even inside our own verified namespace, treat a
        // symlink entry as hostile — `symlink_metadata` does NOT follow, so a link planted here can
        // never redirect `remove_dir_all` at an unrelated target. Only real directories are swept.
        let Ok(metadata) = std::fs::symlink_metadata(&path) else { continue };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let old_enough = metadata
            .modified()
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age >= SWEEP_MAX_AGE);
        if old_enough {
            // Ignore errors: another worker may be removing the same stale dir concurrently.
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Refuse a caller-supplied scratch `tag` that is not exactly one normal path component (#732
/// review, skakri).
///
/// [`scratch_dir`] embeds the tag in the scratch dir name it joins onto the namespace root and
/// then `remove_dir_all`s — a tag carrying a separator, `.`/`..`, a root, or a Windows prefix
/// would let that join escape the dedicated namespace and delete outside it. Requiring a single
/// [`Component::Normal`] and nothing else rejects separators, absolute paths, `..`, `.`, the
/// empty string, and Windows prefixes under both OS path grammars, so no `cfg` split is needed.
/// Panic-loud, matching the module's other security checks ([`ensure_secure_root`]).
fn validate_tag(tag: &str) {
    let mut components = Path::new(tag).components();
    assert!(
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none(),
        "scratch tag must be a single path component: {tag:?}",
    );
}

/// A unique scratch directory PATH under [`scratch_root`], keyed by `tag`, the PID, and a
/// process-wide counter. The path is NOT created (callers create it, matching the fixtures this
/// replaces); any pre-existing dir at the exact path is removed first.
///
/// `tag` must be exactly one normal path component — no separators, no `.`/`..`, not absolute,
/// not empty — and anything else PANICS before any path is built or deleted, so a hostile tag can
/// never steer the join/`remove_dir_all` pair outside the scratch namespace (#732 review).
///
/// On the first call per process this also sweeps stale sibling scratch dirs out of the root (see
/// module docs) — the self-healing backstop for `Drop` cleanup that a `kill -9` skips.
///
/// The root is created and its identity verified via [`ensure_secure_root`] before any sweep, so a
/// namespace an attacker pre-planted as a symlink (or that another user owns) panics the suite
/// rather than being swept.
pub fn scratch_dir(tag: &str) -> PathBuf {
    validate_tag(tag);

    let root = scratch_root();
    ensure_secure_root(&root);
    SWEEP.call_once(|| sweep_stale(&root));

    let name = format!("{tag}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
    let dir = aliased_root(&root).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Name of the self-referential symlink inside the namespace that makes every handed-out scratch
/// path NON-CANONICAL on Unix (see [`aliased_root`]). Not a valid scratch dir name itself —
/// [`scratch_dir`] always appends `-<pid>-<seq>` — so it can never collide with a `tag`.
const NON_CANONICAL_ALIAS: &str = "via-symlink";

/// `root`, reached through a symlinked ancestor so the paths this module hands out are NOT
/// canonical.
///
/// Production's `Config::load` normalizes its root through `canonicalize()`, and index/overlay
/// code depends on that (the worktree overlay derives `config.root`'s subdir by stripping it
/// against a canonicalized repo workdir). On macOS the system temp is already non-canonical
/// (`/var` → `/private/var`) and on Windows it expands 8.3 names like `RUNNER~1`, so a fixture that
/// keeps its own copy of the scratch path is comparing against a root production never sees.
/// Linux was the one platform where the two spellings coincided — which is exactly why that class
/// of bug reached the macOS/Windows legs instead of the per-PR Linux matrix (#1027).
///
/// `<namespace>/via-symlink` is a symlink to the namespace itself, so `<namespace>/via-symlink/x`
/// and `<namespace>/x` are the same directory under two spellings — the Linux stand-in for the
/// divergence the other platforms hand over for free. It lives INSIDE the namespace
/// [`ensure_secure_root`] has already verified (a real, euid-owned, `0700` directory), so it adds
/// no attack surface, and [`sweep_stale`] skips symlink entries so it is never followed or swept.
///
/// Unix-only: creating a symlink on Windows needs developer mode or elevation, and Windows has its
/// own non-canonical spelling anyway. Falls back to `root` unchanged if the link cannot be made,
/// so a restricted filesystem degrades to the old behavior rather than failing the suite.
fn aliased_root(root: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        let alias = root.join(NON_CANONICAL_ALIAS);
        // Racy by construction (every test process runs this): tolerate an already-created link.
        if std::fs::symlink_metadata(&alias).is_err() {
            let _ = std::os::unix::fs::symlink(".", &alias);
        }
        if std::fs::symlink_metadata(&alias).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return alias;
        }
    }
    root.to_path_buf()
}

/// `root` in the canonical form a `Config` loaded from disk would carry.
///
/// `Config::load` normalizes its root through `canonicalize()`, and index/overlay code depends on
/// that: the worktree overlay derives `config.root`'s subdir by stripping it against a
/// canonicalized repo workdir, so a hand-built `Config` holding a non-canonical root strips to
/// nothing and scopes the refresh at the repo root instead of the config root. A fixture that
/// builds its `Config` by hand must therefore canonicalize the same way — otherwise it exercises a
/// configuration production can never produce, and the mis-scoping stays invisible (#1027).
///
/// Fixtures hand roots over before creating them (a `git worktree add` destination must not
/// exist), which plain `canonicalize()` rejects, so this canonicalizes the longest EXISTING
/// ancestor and re-joins the remainder. For a root that exists it is exactly `canonicalize()`;
/// with no existing ancestor at all it returns `root` unchanged.
pub fn canonical_config_root(root: impl Into<PathBuf>) -> PathBuf {
    let root = root.into();
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut probe = root.as_path();
    loop {
        if let Ok(canonical) = probe.canonicalize() {
            return tail.iter().rev().fold(canonical, |acc, part| acc.join(part));
        }
        let (Some(parent), Some(name)) = (probe.parent(), probe.file_name()) else {
            return root;
        };
        tail.push(name);
        probe = parent;
    }
}

/// One uniquely owned scratch directory, removed on drop.
///
/// Names retain [`scratch_dir`]'s tag + PID + atomic-sequence scheme, so parallel tests never share
/// a directory. The guard is deliberately not cloneable: exactly one owner removes each path.
/// Keep it alive until every file, database connection, watcher, and child process using the path
/// has dropped; this is required for cleanup on Windows.
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub fn new(tag: &str) -> Self {
        let path = scratch_dir(tag);
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("failed to create scratch directory {path:?}: {error}"));
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

/// Path ergonomics for fixtures: `scratch.join(...)`, `scratch.canonicalize()`, and `&scratch`
/// where a `&Path` is expected all work directly on the guarded directory.
impl std::ops::Deref for ScratchDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path()
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use filetime::{FileTime, set_file_mtime};

    use super::*;

    /// New scratch dirs must land inside the ONE dedicated, PER-USER namespace sub-root of the
    /// system temp — never scattered directly across the shared temp root (which is what let 16k
    /// dirs accumulate, and what would make the sweep unsafe). The per-user suffix (euid on Unix,
    /// sanitized username on Windows) is what stops two users sharing the temp from colliding on
    /// a fixed, attackable path. Bites if creation is pointed back at the bare temp dir OR if the
    /// namespace is reverted to the un-suffixed shared name.
    #[test]
    fn scratch_dirs_live_under_the_dedicated_namespace() {
        let root = scratch_root();
        // The namespace name carries this process's per-user tag (euid on Unix, sanitized
        // username on Windows) — a shared/un-suffixed name is attackable on a multi-user temp, so
        // pin the exact per-user form.
        let expected = format!("{SCRATCH_NAMESPACE_PREFIX}-{}", namespace_user_tag());
        assert_eq!(
            root.file_name(),
            Some(OsStr::new(&expected)),
            "scratch root should be the dedicated per-user namespace {expected:?}, got {root:?}",
        );
        // The namespace is a CHILD of the system temp, not the bare temp root itself (a bare-root
        // sweep could delete unrelated files).
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        assert_ne!(root, std::env::temp_dir());

        let dir = ScratchDir::new("location-probe");
        assert!(dir.path().starts_with(&root), "{dir:?} is not under the scratch root {root:?}");
    }

    /// The startup sweep must delete a stranded (aged) sibling while sparing a fresh one — the
    /// self-healing backstop for a `kill -9` that skipped `Drop`. Uses an isolated sub-root so
    /// parallel test processes can't perturb the assertion. Bites if `sweep_stale` is neutered.
    #[test]
    fn sweep_removes_stale_dirs_but_spares_fresh_ones() {
        // A private root for this test only (unique via the guard), so no other worker's sweep
        // or dirs interfere with what we assert.
        let test_root = ScratchDir::new("sweep-probe-root");

        let stale = test_root.path().join("stranded-by-kill");
        let fresh = test_root.path().join("live-worker");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();

        // Age the stale dir well past the threshold; leave `fresh` at its just-created mtime.
        let aged = SystemTime::now() - (SWEEP_MAX_AGE + Duration::from_secs(3600));
        set_file_mtime(&stale, FileTime::from_system_time(aged)).unwrap();

        sweep_stale(test_root.path());

        assert!(!stale.exists(), "sweep should have removed the stale (aged) dir {stale:?}");
        assert!(fresh.exists(), "sweep must NOT remove the fresh dir {fresh:?}");
    }

    /// A namespace an attacker pre-planted as a **symlink to a victim directory** must be REFUSED
    /// (panic), and the victim's aged contents must SURVIVE — never swept. This is the exact #726
    /// attack: without the `symlink_metadata` verification, `create_dir_all` accepts the symlink
    /// and the sweep traverses the target, deleting its aged subdirs. Bites if that
    /// verification is removed (no panic AND the victim's aged dir gets swept).
    #[cfg(unix)] // plants a symlink via `std::os::unix::fs::symlink`
    #[test]
    fn refuses_symlinked_namespace_and_spares_victim() {
        use std::os::unix::fs::symlink;

        let parent = ScratchDir::new("symlink-attack-parent");

        // The victim: a directory (owned by us) containing an aged subdir a naive sweep would nuke.
        let victim = parent.path().join("victim");
        let victim_aged = victim.join("precious-aged-data");
        std::fs::create_dir_all(&victim_aged).unwrap();
        let aged = SystemTime::now() - (SWEEP_MAX_AGE + Duration::from_secs(3600));
        set_file_mtime(&victim_aged, FileTime::from_system_time(aged)).unwrap();

        // The attacker pre-creates the namespace path as a symlink to the victim.
        let planted = parent.path().join("planted-namespace");
        symlink(&victim, &planted).unwrap();

        // Drive the real startup sequence (create+verify, then sweep) against the planted path. The
        // verification must panic before the sweep can touch the target.
        let planted_for_closure = planted.clone();
        let result = std::panic::catch_unwind(move || {
            ensure_secure_root(&planted_for_closure);
            sweep_stale(&planted_for_closure);
        });

        assert!(
            result.is_err(),
            "ensure_secure_root must panic on a symlinked namespace {planted:?}, but it returned",
        );
        assert!(
            victim_aged.exists(),
            "the symlink-planting attack deleted the victim's aged dir {victim_aged:?}",
        );
    }

    /// A namespace that is a real directory but owned by ANOTHER user must be refused — a same-user
    /// attacker cannot forge ownership, but a directory we do not own is exactly what we must never
    /// sweep. Creating a foreign-owned dir needs root (to `chown`), so this is a no-op when not run
    /// as root. Bites if the euid-ownership check is dropped.
    #[cfg(unix)] // the ownership check itself is Unix-only; needs `libc::chown`
    #[test]
    fn refuses_foreign_owned_namespace() {
        use std::os::unix::ffi::OsStrExt;

        // `chown` to another uid requires privilege; only root can set this up. Skip otherwise
        // (CI here runs as root, where the check is exercised for real).
        if current_euid() != 0 {
            eprintln!("skipping foreign-owner test: not root (euid={})", current_euid());
            return;
        }

        let parent = ScratchDir::new("foreign-owner-parent");
        let namespace = parent.path().join("foreign-namespace");
        std::fs::create_dir_all(&namespace).unwrap();

        // Hand the namespace to `nobody` (65534) so it is a real dir we no longer own.
        let c_path = std::ffi::CString::new(namespace.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path for the duration of the call.
        let rc = unsafe { libc::chown(c_path.as_ptr(), 65534, 65534) };
        assert_eq!(rc, 0, "chown to nobody failed: {}", std::io::Error::last_os_error());

        let namespace_for_closure = namespace.clone();
        let result = std::panic::catch_unwind(move || ensure_secure_root(&namespace_for_closure));

        assert!(
            result.is_err(),
            "ensure_secure_root must panic on a foreign-owned namespace {namespace:?}",
        );
    }

    /// Assert `scratch_dir` refuses `tag` by panicking with the single-component message —
    /// `catch_unwind`, the module's expected-panic pattern. Checking the payload proves the panic
    /// is the tag validation, not an unrelated filesystem failure.
    fn assert_tag_refused(tag: &str) {
        let result = std::panic::catch_unwind(|| scratch_dir(tag));
        let Err(payload) = result else {
            panic!("scratch_dir must refuse the non-single-component tag {tag:?}, but it returned");
        };
        let msg = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&'static str>().copied())
            .unwrap_or("<non-string panic payload>");
        assert!(
            msg.contains("scratch tag must be a single path component"),
            "unexpected panic message for tag {tag:?}: {msg}",
        );
    }

    /// A tag carrying a path separator must be refused (#732 review, skakri): joined onto the
    /// namespace root it would nest the scratch dir below an attacker-chosen subdirectory, and the
    /// `remove_dir_all` in `scratch_dir` would then delete outside the dedicated namespace.
    #[test]
    fn refuses_tag_with_separator() {
        assert_tag_refused("nested/evil");
    }

    /// An absolute-path tag must be refused: `Path::join` with an absolute path REPLACES the
    /// namespace root, so the scratch dir — and its deletion — would land anywhere on disk.
    #[test]
    fn refuses_absolute_tag() {
        assert_tag_refused("/tmp/evil");
    }

    /// A `..` tag must be refused: it resolves the join to the namespace's PARENT (the shared
    /// system temp), so `remove_dir_all` would aim at a sibling of the namespace.
    #[test]
    fn refuses_parent_dir_tag() {
        assert_tag_refused("..");
    }

    /// An empty tag must be refused: it is zero path components, not one.
    #[test]
    fn refuses_empty_tag() {
        assert_tag_refused("");
    }

    /// A normal single-component tag keeps its exact pre-validation behavior: a fresh path
    /// directly under the namespace root, named `<tag>-<pid>-<seq>`.
    #[test]
    fn accepts_normal_single_component_tag() {
        let dir = ScratchDir::new("normal-tag");
        // The parent is the namespace, reached through the non-canonical alias on Unix.
        assert_eq!(dir.path().parent(), Some(aliased_root(&scratch_root()).as_path()));
        assert_eq!(canonical_config_root(dir.path()).parent(), Some(scratch_root().as_path()));
        let name = dir.path().file_name().and_then(|n| n.to_str()).unwrap_or_default();
        assert!(name.starts_with("normal-tag-"), "unexpected scratch name {name:?}");
    }

    /// The scratch paths this module hands out must NOT be canonical on Unix, so a Linux run
    /// exercises the same root divergence macOS (`/var` → `/private/var`) and Windows (8.3
    /// `RUNNER~1`) produce by default — the divergence that let a mis-scoped worktree overlay
    /// reach the cross-platform legs unnoticed (#1027). Bites if [`aliased_root`] is neutered:
    /// the class would silently stop being covered on the per-PR matrix.
    #[cfg(unix)]
    #[test]
    fn scratch_paths_are_not_canonical_so_linux_covers_the_macos_root_divergence() {
        let dir = ScratchDir::new("alias-probe");
        let canonical = canonical_config_root(dir.path());
        assert_ne!(
            canonical,
            dir.path(),
            "scratch paths must reach the directory through a symlinked ancestor",
        );
        // Same directory, two spellings: a file written through one is visible through the other.
        std::fs::write(dir.path().join("probe"), "payload").unwrap();
        assert_eq!(std::fs::read_to_string(canonical.join("probe")).unwrap(), "payload");
        assert!(canonical.starts_with(scratch_root()), "the canonical form stays in the namespace");
    }

    /// [`canonical_config_root`] must match `Config::load`'s `canonicalize()` for an EXISTING root
    /// and still resolve the symlinked ancestors of a root that does not exist yet — fixtures hand
    /// over `git worktree add` destinations before creating them.
    #[cfg(unix)]
    #[test]
    fn canonical_config_root_resolves_an_absent_leaf_through_its_symlinked_ancestors() {
        let dir = ScratchDir::new("absent-leaf-probe");
        let existing = canonical_config_root(dir.path());
        assert_eq!(existing, dir.path().canonicalize().unwrap());

        let absent = dir.path().join("not-created-yet/nested");
        assert_eq!(canonical_config_root(&absent), existing.join("not-created-yet/nested"));
        // Once it exists, plain canonicalization agrees.
        std::fs::create_dir_all(&absent).unwrap();
        assert_eq!(canonical_config_root(&absent), absent.canonicalize().unwrap());
    }

    #[test]
    fn scratch_guard_removes_its_directory_on_drop() {
        let path = {
            let scratch = ScratchDir::new("drop-probe");
            let path = scratch.path().to_path_buf();
            std::fs::write(path.join("payload"), "test").unwrap();
            path
        };
        assert!(!path.exists(), "scratch guard left its directory behind: {path:?}");
    }

    #[test]
    fn scratch_guard_removes_its_directory_during_unwind() {
        let scratch = ScratchDir::new("unwind-probe");
        let path = scratch.path().to_path_buf();
        let result = std::panic::catch_unwind(move || {
            let _scratch = scratch;
            panic!("fixture failure");
        });
        assert!(result.is_err());
        assert!(!path.exists(), "unwinding left the scratch directory behind: {path:?}");
    }

    #[test]
    fn parallel_scratch_guards_get_distinct_directories() {
        let workers = (0..32)
            .map(|_| std::thread::spawn(|| ScratchDir::new("parallel-probe")))
            .collect::<Vec<_>>();
        let scratch = workers.into_iter().map(|worker| worker.join().unwrap()).collect::<Vec<_>>();
        let paths = scratch
            .iter()
            .map(|dir| dir.path().to_path_buf())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(paths.len(), scratch.len());
        assert!(paths.iter().all(|path| path.is_dir()));

        drop(scratch);
        assert!(paths.iter().all(|path| !path.exists()));
    }

    /// The exact #732-review attack: a `..` tag aims the join+delete at a victim OUTSIDE the
    /// namespace. The victim is pre-created at the precise path the un-validated code would
    /// delete — predicted from this process's PID and the next SEQ value (nextest runs each test
    /// in its own process, so this test owns the counter) — so dropping the validation fails this
    /// test by losing the victim, not just by returning. The call must REFUSE and the victim must
    /// SURVIVE. Mirror of `refuses_symlinked_namespace_and_spares_victim`.
    #[test]
    fn refuses_escaping_tag_and_spares_victim() {
        // Predict the exact name `scratch_dir` will build for the malicious call below:
        // `{tag}-{pid}-{seq}`. The join resolves `root/../<predicted>` to a sibling of the
        // namespace in the shared temp — plant the victim there so the attack has a real target.
        let tag = "../victim-escape";
        let predicted =
            format!("victim-escape-{}-{}", std::process::id(), SEQ.load(Ordering::Relaxed));
        let victim = std::env::temp_dir().join(predicted);
        let victim_precious = victim.join("precious-data");
        std::fs::create_dir_all(&victim_precious).unwrap();

        let result = std::panic::catch_unwind(|| scratch_dir(tag));

        assert!(
            victim_precious.exists(),
            "the escaping tag deleted the victim dir {victim:?} outside the scratch namespace",
        );
        assert!(
            result.is_err(),
            "scratch_dir must refuse the escaping tag {tag:?}, but it returned"
        );

        // The victim lives outside the swept namespace, so clean it up explicitly.
        let _ = std::fs::remove_dir_all(&victim);
    }
}
