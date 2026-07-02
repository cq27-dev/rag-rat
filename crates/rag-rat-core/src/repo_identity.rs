//! Machine-stable repo identity: a content-derived `repo_id` (the repo's root-commit hash) plus a
//! cosmetic display name. This id is what the consolidated global database scopes every repo's
//! rows and memories by (memory-sync phase A). It is deliberately derived from the commit graph,
//! not the on-disk path, so worktrees, clones, and re-clones of the same repo share one identity.

use std::path::Path;

use crate::index::schema::LEGACY_REPO_ID;

/// A repo's identity for the registry: the scoping key + a human label.
#[derive(Debug, Clone)]
pub struct RepoIdentity {
    /// Machine-stable, content-derived (root-commit hash) unless pinned via `rag-rat.toml`. Never
    /// derived from the on-disk path.
    pub repo_id: String,
    /// The working-tree directory name — cosmetic only, never an identity input.
    pub display_name: String,
}

/// Resolve a repo's identity from its working tree at `root`.
///
/// The default `repo_id` is the **lexicographically smallest root-commit hash** reachable from
/// HEAD. A linear history has exactly one root (parentless) commit; a history stitched from
/// several orphan branches has several, and choosing the smallest by byte order makes the id
/// deterministic across machines regardless of which root the walk reaches first.
///
/// `override_id` (from `rag-rat.toml`'s `[index] repo_id`) wins outright when set and non-empty —
/// the escape hatch for forks that must NOT share identity with their upstream, and for pinning an
/// id on a repo whose root is unreachable (an empty repo, or a shallow clone that cut history).
///
/// Errors — each with an actionable remedy — when no override was supplied and the id cannot be
/// derived: the directory is not a git repository, the repo has no commits (unborn HEAD), or its
/// history is a cut shallow clone (the root is unreachable, so any derived id would depend on clone
/// depth and split identity across machines).
pub fn resolve_repo_identity(
    root: &Path,
    override_id: Option<&str>,
) -> anyhow::Result<RepoIdentity> {
    let display_name =
        root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();

    if let Some(id) = override_id.map(str::trim).filter(|id| !id.is_empty()) {
        // The placeholder marker is reserved for a pre-adoption single-repo DB; pinning it would
        // degenerate registry adoption (see `register_repo`). Refuse with a remedy-naming error
        // rather than mint an identity that can never be adopted.
        if id == LEGACY_REPO_ID {
            anyhow::bail!(reserved_repo_id_error());
        }
        return Ok(RepoIdentity { repo_id: id.to_string(), display_name });
    }

    let repo_id = smallest_root_commit(root)?;
    Ok(RepoIdentity { repo_id, display_name })
}

/// The remedy-naming error for pinning the reserved placeholder id: it marks a pre-adoption
/// single-repo DB, so adopting under it would rewrite the placeholder PK to itself and leave the DB
/// unadopted while reporting success (see [`register_repo`]).
///
/// [`register_repo`]: crate::index::schema::register_repo
fn reserved_repo_id_error() -> String {
    format!(
        "`{LEGACY_REPO_ID}` is a reserved repo_id (the pre-adoption placeholder marker) and \
         cannot be pinned via `[index] repo_id` in rag-rat.toml. Choose a different stable \
         string, or omit the override to derive the id from the root commit."
    )
}

/// The lexicographically smallest hash among the parentless commits reachable from HEAD. Mirrors
/// the git-history walk (`index::git_history`): discover the repo, resolve HEAD, `rev_walk`, and
/// keep the commits whose first parent is absent.
///
/// Refuses (with an actionable error) when no parentless root is reachable — the exact signature of
/// a shallow clone that CUT history: gix reads parents from the commit object and does not apply
/// shallow grafts, so the boundary commit reports parents whose objects are absent (never a root),
/// while the walk itself honors the boundary and yields no true root. Deriving an id from that
/// boundary would make identity depend on clone depth, so we reject rather than mint a
/// depth-varying id. A shallow clone whose depth COVERS the whole history keeps its true root
/// reachable and resolves normally — hence the "no reachable root" signal rather than
/// `repo.is_shallow()` alone, which is also set for those fully-present clones.
fn smallest_root_commit(root: &Path) -> anyhow::Result<String> {
    let repo = crate::index::discover_repo(root)?;
    // Unborn HEAD (`git init` with no commits) has no history to derive an id from.
    let Ok(head) = repo.head_id() else {
        anyhow::bail!(empty_repo_error(root));
    };
    let head_id = head.detach();

    let mut roots: Vec<String> = Vec::new();
    for info in repo.rev_walk([head_id]).all()? {
        let info = info?;
        let commit = repo.find_commit(info.id)?;
        // A root commit has no parents; `parent_ids().next().is_none()` is the same predicate the
        // history walk uses to detect roots. A cut shallow boundary reports (absent) parents, so it
        // is deliberately NOT counted here.
        if commit.parent_ids().next().is_none() {
            roots.push(info.id.to_hex().to_string());
        }
    }
    roots.sort();
    if let Some(smallest) = roots.into_iter().next() {
        return Ok(smallest);
    }

    // No reachable parentless root: a cut shallow clone (the common case) or a corrupt object
    // graph.
    if repo.is_shallow() {
        anyhow::bail!(shallow_clone_error(root));
    }
    anyhow::bail!("repository at {} has no reachable root commit", root.display());
}

/// The remedy-naming error for a shallow clone that cut history: identity would depend on clone
/// depth, so refuse and point at the two fixes (unshallow, or pin the id).
fn shallow_clone_error(root: &Path) -> String {
    format!(
        "cannot derive a stable repo_id from the shallow clone at {}: its history is cut, so the \
         root commit is unreachable and any derived id would depend on clone depth (splitting \
         this repo's identity across machines). Run `git fetch --unshallow`, or pin `[index] \
         repo_id = \"…\"` in rag-rat.toml.",
        root.display()
    )
}

/// The remedy-naming error for an empty repo (unborn HEAD): there is no commit graph to derive
/// from.
fn empty_repo_error(root: &Path) -> String {
    format!(
        "cannot derive a repo_id: the repository at {} has no commits (unborn HEAD). Make a \
         commit, or pin `[index] repo_id = \"…\"` in rag-rat.toml.",
        root.display()
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let mut root = std::env::temp_dir();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        root.push(format!("rag-rat-repo-identity-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn git_out(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn init_repo(root: &Path) {
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.email", "t@e"]);
        git(root, &["config", "user.name", "t"]);
    }

    /// All root (parentless) commit hashes reachable from HEAD, sorted — the ground truth the
    /// resolver's "smallest root commit" must agree with, computed independently via git.
    fn root_hashes(root: &Path) -> Vec<String> {
        let mut hashes: Vec<String> = git_out(root, &["rev-list", "--max-parents=0", "HEAD"])
            .lines()
            .map(str::to_string)
            .collect();
        hashes.sort();
        hashes
    }

    #[test]
    fn single_root_repo_resolves_to_its_root_commit() {
        let root = temp_root();
        init_repo(&root);
        git(&root, &["commit", "--allow-empty", "-q", "-m", "genesis"]);

        let expected = root_hashes(&root);
        assert_eq!(expected.len(), 1, "a linear history has exactly one root");

        let identity = resolve_repo_identity(&root, None).unwrap();
        assert_eq!(identity.repo_id, expected[0]);
        assert_eq!(identity.display_name, root.file_name().unwrap().to_string_lossy());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn multi_root_history_resolves_to_lexicographically_smallest() {
        let root = temp_root();
        init_repo(&root);
        // Root A on main, then an unrelated orphan root B, then merge so HEAD reaches BOTH.
        git(&root, &["commit", "--allow-empty", "-q", "-m", "root-a"]);
        git(&root, &["checkout", "-q", "--orphan", "orphan-b"]);
        git(&root, &["commit", "--allow-empty", "-q", "-m", "root-b"]);
        git(&root, &["checkout", "-q", "main"]);
        git(&root, &["merge", "-q", "--allow-unrelated-histories", "--no-edit", "orphan-b"]);

        let roots = root_hashes(&root);
        assert_eq!(roots.len(), 2, "merged orphan histories give two roots");

        let identity = resolve_repo_identity(&root, None).unwrap();
        assert_eq!(identity.repo_id, roots[0], "smallest root hash by byte order wins");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn override_wins_over_derived_id() {
        let root = temp_root();
        init_repo(&root);
        git(&root, &["commit", "--allow-empty", "-q", "-m", "genesis"]);
        let derived = root_hashes(&root).remove(0);

        let identity = resolve_repo_identity(&root, Some("  pinned-id  ")).unwrap();
        assert_eq!(identity.repo_id, "pinned-id", "override is trimmed and wins");
        assert_ne!(identity.repo_id, derived);

        // An empty/whitespace override falls through to the derived id.
        let blank = resolve_repo_identity(&root, Some("   ")).unwrap();
        assert_eq!(blank.repo_id, derived);
        std::fs::remove_dir_all(&root).ok();
    }

    /// The pre-adoption placeholder marker ([`LEGACY_REPO_ID`]) is a RESERVED repo_id: pinning it
    /// via `[index] repo_id` would degenerate registry adoption (the adoption UPDATE rewrites the
    /// placeholder PK to itself, leaving the DB unadopted while reporting success). The resolver
    /// refuses it — before and after trimming — with an error naming the reserved value.
    #[test]
    fn override_equal_to_the_reserved_placeholder_is_refused() {
        let root = temp_root();
        init_repo(&root);
        git(&root, &["commit", "--allow-empty", "-q", "-m", "genesis"]);

        let err = resolve_repo_identity(&root, Some(LEGACY_REPO_ID))
            .expect_err("the reserved placeholder id must not be pinnable");
        let msg = err.to_string();
        assert!(msg.contains(LEGACY_REPO_ID), "error names the reserved value: {msg}");
        assert!(msg.contains("reserved"), "error explains it is reserved: {msg}");

        // Whitespace padding must not sneak the marker past the trim.
        let padded = format!("  {LEGACY_REPO_ID}  ");
        let err_padded = resolve_repo_identity(&root, Some(&padded))
            .expect_err("the trimmed reserved id is still refused");
        assert!(err_padded.to_string().contains("reserved"), "{err_padded}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Build an origin repo with `commits` empty commits under `base/origin`, then a `--depth`
    /// clone of it at `base/<dest>`. Returns `(clone_root, origin_root_hash)`.
    fn shallow_clone(base: &Path, commits: usize, depth: usize, dest: &str) -> (PathBuf, String) {
        let origin = base.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        init_repo(&origin);
        for i in 0..commits {
            git(&origin, &["commit", "--allow-empty", "-q", "-m", &format!("c{i}")]);
        }
        let origin_root = root_hashes(&origin).remove(0);
        let url = format!("file://{}", origin.display());
        git(base, &["clone", "-q", "--depth", &depth.to_string(), &url, dest]);
        (base.join(dest), origin_root)
    }

    /// A genuinely-cut shallow clone (`--depth` < history) has no reachable parentless root: the
    /// boundary commit records parents whose objects are absent. Deriving an id from the boundary
    /// would make identity depend on clone depth and split it across machines — so it is REFUSED,
    /// with an error naming both remedies (`git fetch --unshallow` or a pinned `repo_id`).
    #[test]
    fn shallow_clone_that_cuts_history_is_refused_with_an_actionable_error() {
        let base = temp_root();
        let (shallow, _origin_root) = shallow_clone(&base, 5, 1, "shallow");
        // Sanity: the fixture is really a shallow clone that cut history.
        assert!(crate::index::discover_repo(&shallow).unwrap().is_shallow());

        let err = resolve_repo_identity(&shallow, None)
            .expect_err("a depth-cut shallow clone must not derive a depth-dependent id");
        let msg = err.to_string();
        assert!(msg.contains("shallow"), "error names the cause: {msg}");
        assert!(msg.contains("git fetch --unshallow"), "error names the unshallow remedy: {msg}");
        assert!(msg.contains("repo_id"), "error names the pin remedy: {msg}");

        // The override escape hatch still lets a shallow clone be pinned deterministically.
        let pinned = resolve_repo_identity(&shallow, Some("pinned-shallow")).unwrap();
        assert_eq!(pinned.repo_id, "pinned-shallow");
        std::fs::remove_dir_all(&base).ok();
    }

    /// A clone flagged shallow whose `--depth` COVERS the whole history is NOT depth-dependent: the
    /// boundary commit IS the true root (all its ancestors are present), so identity resolves to
    /// the real root hash and matches the origin. `is_shallow()` alone would wrongly reject
    /// this; the "no reachable parentless root" signal correctly accepts it.
    #[test]
    fn shallow_clone_covering_full_history_resolves_the_real_root() {
        let base = temp_root();
        // depth == commit count: git still writes `.git/shallow`, but nothing is actually cut.
        let (shallow, origin_root) = shallow_clone(&base, 5, 5, "shallow");
        assert!(
            crate::index::discover_repo(&shallow).unwrap().is_shallow(),
            "git flags a depth-exact clone shallow"
        );

        let identity = resolve_repo_identity(&shallow, None)
            .expect("full history present → the real root is reachable");
        assert_eq!(identity.repo_id, origin_root, "id is the real root, depth-independent");
        std::fs::remove_dir_all(&base).ok();
    }

    /// An empty repo (`git init`, unborn HEAD, zero commits) cannot derive an id; the error names
    /// the remedy (pin a `repo_id`) instead of surfacing a raw gix "unborn HEAD" error.
    #[test]
    fn empty_repo_without_override_is_an_actionable_error() {
        let root = temp_root();
        init_repo(&root); // git init, but no commits

        let err = resolve_repo_identity(&root, None)
            .expect_err("an empty repo has no commit graph to derive an id from");
        let msg = err.to_string();
        assert!(msg.contains("no commits"), "error names the cause: {msg}");
        assert!(msg.contains("repo_id"), "error names the pin remedy: {msg}");

        // A pin still lets an empty repo be registered.
        let pinned = resolve_repo_identity(&root, Some("pinned-empty")).unwrap();
        assert_eq!(pinned.repo_id, "pinned-empty");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn non_git_dir_without_override_is_an_error() {
        let root = temp_root(); // created, but never `git init`ed
        assert!(resolve_repo_identity(&root, None).is_err());
        // ...but an override lets a non-git dir resolve (a repo with no commits can still be
        // pinned).
        let identity = resolve_repo_identity(&root, Some("pinned")).unwrap();
        assert_eq!(identity.repo_id, "pinned");
        std::fs::remove_dir_all(&root).ok();
    }
}
