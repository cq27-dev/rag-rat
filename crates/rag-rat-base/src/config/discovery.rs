use std::path::{Path, PathBuf};

use super::ConfigError;

/// The DEFAULT config path for the checkout containing `dir` — the DISCOVERY side of the
/// governing seam. [`Config::load`]'s seam decides which config WINS once a file is loaded; this
/// decides where the CLI LOOKS when no explicit `--config` was given. Without it, a linked
/// worktree with no branch-local `rag-rat.toml` (the state `init`'s refusal deliberately leaves)
/// dies at the existence check before the seam ever runs (Codex batch 9). Resolution:
///  * a local `rag-rat.toml` exists → use it. In a linked worktree the seam then governs from main
///    AND emits the divergence warning — routing discovery straight to main when a branch file
///    exists would silently skip that warning, so the file's presence keeps the load going through
///    the seam (governance is identical either way).
///  * linked worktree, no local file at `dir` → the nearest `rag-rat.toml` in an ANCESTOR up to the
///    linked worktree root (a subdirectory launch still finds a branch-local config → seam +
///    warning); failing that, the MAIN worktree's `rag-rat.toml` (whether or not it exists yet — a
///    missing-config hint should name where the config BELONGS).
///  * main/non-git, no local file → the nearest `rag-rat.toml` in an ANCESTOR directory (bounded at
///    the enclosing git root so a nested checkout never adopts a parent repo's config), so a launch
///    from a SUBDIRECTORY of a rag-rat repo still finds the repo's config; failing that, the local
///    path (the hint names it).
///
/// An EXPLICIT `--config` path never routes through this — a user override is taken literally.
pub fn discover_config_path(dir: &Path) -> PathBuf {
    let local = dir.join("rag-rat.toml");
    if local.exists() {
        return local;
    }
    match linked_worktree_main_root(dir) {
        // Linked worktree: a BRANCH-LOCAL rag-rat.toml (at the worktree root, or an ancestor of
        // `dir` within it) must still be found from a SUBDIRECTORY launch — it routes the load
        // through the governing seam, which governs from main AND emits the divergence warning
        // (jumping straight to main would skip that warning, or wrongly go dormant when main has no
        // config). `nearest_config_at_or_above` is bounded at the enclosing git root (the linked
        // worktree root), so it cannot escape into main or a parent. No branch-local config
        // ANYWHERE in the linked worktree ⇒ main's config path (even if missing) — the
        // governing-seam invariant.
        Some(main_top) =>
            nearest_config_at_or_above(dir).unwrap_or_else(|| main_top.join("rag-rat.toml")),
        None => nearest_config_at_or_above(dir).unwrap_or(local),
    }
}

/// Walk upward from `dir` to the nearest directory (at or above `dir`) holding a `rag-rat.toml`,
/// returning that file's path. `None` ⇒ no rag-rat repo at or above `dir`. The single upward-walk
/// primitive: `discover_config_path`'s non-worktree arm uses it so a subdirectory launch inside a
/// repo finds the repo's config, and the Claude-hook cwd→config resolver
/// (`agent_hook::find_config`) loads the returned path.
///
/// The climb STOPS at the enclosing git repository root: a nested checkout or submodule that has no
/// `rag-rat.toml` of its own must NOT bind to an indexed PARENT repo's config — that would target
/// the wrong repository for searches and, worse, for memory writes. When `dir` is not inside a git
/// repo there is no such boundary, so the walk runs to the filesystem root.
pub fn nearest_config_at_or_above(dir: &Path) -> Option<PathBuf> {
    // Resolve to an ABSOLUTE path first. A relative `dir` such as `.` has `parent() == Some("")`
    // then `None`, so the ancestor walk would only ever inspect `.` and never climb the real
    // filesystem tree (the callers pass `Path::new(".")` for cwd). `canonicalize` also collapses
    // `..`/symlinks so the climb follows the true directory chain. If it fails (a non-existent
    // `dir`), fall back to walking `dir` as given rather than aborting discovery.
    let absolute = dir.canonicalize().ok();
    let start = absolute.as_deref().unwrap_or(dir);
    // The enclosing git repo's workdir root — the ceiling the climb must not cross (canonicalized
    // so it compares equal to the canonicalized `cur`). `None` for a non-git `dir` ⇒ no
    // ceiling.
    let boundary = crate::repo_discover::discover_repo(start)
        .ok()
        .and_then(|repo| repo.workdir().map(Path::to_path_buf))
        .and_then(|workdir| workdir.canonicalize().ok());
    let mut current = Some(start);
    while let Some(cur) = current {
        let candidate = cur.join("rag-rat.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        // At the git repo root: its `rag-rat.toml` was just checked; never climb into a parent
        // repo.
        if boundary.as_deref() == Some(cur) {
            break;
        }
        current = cur.parent();
    }
    None
}

/// The MAIN worktree top for `root` when `root` sits in a LINKED git worktree — `None` when the
/// checkout containing `root` IS the main worktree (or the layout has no designated main:
/// bare-repo hubs, custom `GIT_DIR`, non-git dirs). This is THE linked-ness predicate — derived
/// from git topology (the discovered checkout's WORKDIR vs the common dir's main), never from a
/// path-equality proxy: comparing `root` itself to main falsely classifies a SUBDIRECTORY of the
/// main worktree as linked, and root-anchoring success is defeated by a branch-only `[index]
/// root` (Codex batch 8, findings 1+3). Both `Config::load`'s governing seam and the CLI's
/// `init` refusal resolve linked-ness through this one helper.
pub fn linked_worktree_main_root(root: &Path) -> Option<PathBuf> {
    let repo = crate::repo_discover::discover_repo(root).ok()?;
    let main = main_worktree_root(root)?;
    let workdir = repo.workdir()?;
    let workdir = workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf());
    (main != workdir).then_some(main)
}

/// The git WORKDIR root (checkout top) containing `path`, canonicalized — `None` for a non-git
/// `path`. The SESSION-side counterpart to [`linked_worktree_main_root`] (which returns the MAIN
/// checkout's top): both are needed to rebase a main-anchored `config.root` onto the checkout a
/// session is actually in. Canonicalized so it compares equal to the other topology roots.
pub fn worktree_root(path: &Path) -> Option<PathBuf> {
    let repo = crate::repo_discover::discover_repo(path).ok()?;
    let workdir = repo.workdir()?;
    Some(workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf()))
}

pub(crate) fn main_worktree_root(root: &Path) -> Option<PathBuf> {
    let repo = crate::repo_discover::discover_repo(root).ok()?;
    let common_dir = repo.common_dir().canonicalize().ok()?;
    // Only the standard `<main>/.git` layout maps cleanly to a main worktree root.
    if common_dir.file_name()?.to_str()? != ".git" {
        return None;
    }
    let main_root = common_dir.parent()?.to_path_buf();
    main_root.is_dir().then_some(main_root)
}

/// The database path a `rag-rat.toml` WITHOUT a `database` key resolves to (A7), pure — no logging,
/// no filesystem writes. The default is the CONSOLIDATED GLOBAL store (`data_dir()/rag-rat.sqlite`)
/// — one database per machine — with two exceptions that keep an upgrade safe:
///  1. A pre-existing legacy `<main_worktree>/.rag-rat/index.sqlite` is honored, so a repo indexed
///     before the flip never silently abandons its authored memories; once `rag-rat consolidate`
///     imports and renames the file away, resolution falls through to the global path.
///  2. When no data dir resolves at all (no `HOME`/XDG on this platform), fall back to the legacy
///     per-repo path rather than failing — the pre-A7 behavior.
///
/// `db_base` is the directory a relative/legacy path anchors to (the main worktree top — see
/// `Config::load`). Public so the init wizard can display where a keyless config will land without
/// loading one.
pub fn default_database_path(
    db_base: &Path,
    identity_root: &Path,
    repo_id_override: Option<&str>,
) -> PathBuf {
    let (path, _) = default_database_with_disposition(db_base, identity_root, repo_id_override);
    path
}

/// How a keyless config's default database resolved — the load path warns per variant.
enum DefaultDatabaseDisposition {
    /// The root has NO derivable repo identity (non-git dir, unborn `git init`, no pin): the
    /// per-root legacy path, exactly the pre-flip posture.
    IdentityLess,
    /// The legacy per-repo file is in use (awaiting `rag-rat consolidate`).
    Legacy,
    /// The global store (or the no-data-dir legacy fallback path, which does not exist on disk).
    Global,
    /// The global store, with a STRAY legacy file present DESPITE the `.imported` marker — an old
    /// binary, a backup restore, or a stray process re-created it after consolidation.
    GlobalWithStrayLegacy,
}

/// [`default_database_path`] plus the [`DefaultDatabaseDisposition`] the resolution took.
///
/// IDENTITY GATE (first, before everything): the global default REQUIRES a resolvable repo
/// identity (`repo_identity::identity_is_resolvable` — a pin, or a git repo with a born HEAD).
/// An identity-less root stays on its per-root `.rag-rat/index.sqlite` exactly as pre-flip:
/// in the shared global store every such root would pool under the ONE `__unassigned__`
/// placeholder scope — two fresh non-git projects would see and overwrite each other — and an
/// unborn repo would strand its placeholder rows the moment its first commit mints a real id.
/// Per-root, the placeholder stays a single-repo-DB concept with its existing adoption flow
/// (first commit → the placeholder adopts in the per-root DB → consolidate when ready).
///
/// The `.imported` marker is a STAY-GLOBAL LATCH: once `rag-rat consolidate` has renamed the legacy
/// file away, a keyless repo resolves to the global store even if a legacy `index.sqlite`
/// REAPPEARS beside the marker (an old binary, a restored backup, a stray process) — otherwise the
/// stray would silently divert the repo off the store its memories were imported into.
fn default_database_with_disposition(
    db_base: &Path,
    identity_root: &Path,
    repo_id_override: Option<&str>,
) -> (PathBuf, DefaultDatabaseDisposition) {
    let legacy = db_base.join(".rag-rat/index.sqlite");
    if !crate::repo_identity::identity_is_resolvable(identity_root, repo_id_override) {
        return (legacy, DefaultDatabaseDisposition::IdentityLess);
    }
    let marker = db_base.join(".rag-rat/index.sqlite.imported");
    let global = crate::data_dir::global_database_path();
    if marker.exists()
        && let Some(global) = global
    {
        let disposition = if legacy.exists() {
            DefaultDatabaseDisposition::GlobalWithStrayLegacy
        } else {
            DefaultDatabaseDisposition::Global
        };
        return (global, disposition);
    }
    if legacy.exists() {
        return (legacy, DefaultDatabaseDisposition::Legacy);
    }
    (global.unwrap_or(legacy), DefaultDatabaseDisposition::Global)
}

/// The load-time wrapper around [`default_database_path`]: same resolution, plus a one-line notice
/// per disposition — the deprecation nudge toward `rag-rat consolidate` while the legacy file is
/// what keeps the repo off the global store, and a stray-file warning when a legacy file reappears
/// after consolidation (it is ignored, never silently adopted). An identity-less root is SILENT:
/// it is the pre-flip posture, and `rag-rat consolidate` refuses identity-less repos, so a nudge
/// would dead-end.
pub(crate) fn resolve_default_database(
    db_base: &Path,
    identity_root: &Path,
    repo_id_override: Option<&str>,
) -> PathBuf {
    let (path, disposition) =
        default_database_with_disposition(db_base, identity_root, repo_id_override);
    match disposition {
        DefaultDatabaseDisposition::Legacy => tracing::warn!(
            path = %path.display(),
            "using the legacy per-repo index at `.rag-rat/index.sqlite`; the default database is now \
             the consolidated global store. Run `rag-rat consolidate` to import this repo's memories \
             and switch to it."
        ),
        DefaultDatabaseDisposition::GlobalWithStrayLegacy => tracing::warn!(
            "a stray `.rag-rat/index.sqlite` exists beside the `.imported` consolidation marker; \
             ignoring it and staying on the consolidated global store. Delete the stray file (its \
             contents were NOT imported)."
        ),
        DefaultDatabaseDisposition::Global | DefaultDatabaseDisposition::IdentityLess => {},
    }
    path
}

/// The DEFAULT legacy per-repo path a KEYLESS config at `root` would consult
/// (`<main_worktree_top>/.rag-rat/index.sqlite`) — what `rag-rat consolidate` compares a pinned
/// `database` path against to pick the right remedy: a pin AT this path just needs the key
/// removed, while a CUSTOM pin must also move its file here first (keyless resolution never looks
/// anywhere else, so removing the key alone would strand the custom file unimported).
pub fn default_legacy_database_path(root: &Path) -> PathBuf {
    main_worktree_root(root).unwrap_or_else(|| root.to_path_buf()).join(".rag-rat/index.sqlite")
}

pub(crate) fn normalize_existing_dir(path: &Path) -> Result<PathBuf, ConfigError> {
    let absolute =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let canonical = absolute.canonicalize()?;
    if !canonical.is_dir() {
        return Err(ConfigError::MissingDirectory(canonical));
    }
    Ok(canonical)
}
