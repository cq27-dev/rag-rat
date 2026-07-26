//! Machine-stable repo identity: a content-derived `repo_id` (the repo's root-commit hash) plus a
//! cosmetic display name and a [`RepoIdentityClass`] portability tag. This id is what the
//! consolidated global database scopes every repo's rows and memories by (memory-sync phase A). It
//! is deliberately derived from the commit graph, not the on-disk path, so worktrees, clones, and
//! re-clones of the same repo share one identity.
//!
//! A repo whose full history is present derives a `Portable` id (the same on every machine — the
//! identity peer sync can trust). A shallow clone whose history was CUT cannot reach its root, so
//! it derives a deterministic-but-depth-dependent `LocalOnly` id from the shallow boundary instead
//! of failing: indexing must not be blocked by a shallow checkout (CI fixtures, `--depth 1`
//! clones). The portability class travels with the identity so the future sync layer can refuse to
//! replicate a `LocalOnly` id; open-time never hard-rejects a shallow clone.

use std::path::Path;

/// The pre-adoption placeholder repo id: rows written before a repository identity is
/// registered carry this marker, and schema adoption (`register_repo`) re-points them at the
/// real id. Reserved — never accepted as a user-supplied identity.
pub const LEGACY_REPO_ID: &str = "__unassigned__";

/// The prefix every machine-local [`LocalOnly`](RepoIdentityClass::LocalOnly) id carries (a cut
/// shallow clone's boundary hash). RESERVED: the resolver only ever mints it for a shallow clone,
/// and `register_repo` keys the LocalOnly→Portable upgrade off it (an incumbent id starting with
/// this prefix, being re-pointed to an incoming `Portable` id, is a deepened clone). Pinning a
/// `local:`-prefixed `[index] repo_id` is therefore refused — it would collide with that detection.
pub const LOCAL_ONLY_ID_PREFIX: &str = "local:";

/// Whether a repo's identity is portable across machines (safe for peer sync) or only stable on
/// THIS machine's clone. Open-time resolution NEVER hard-rejects a shallow clone — it downgrades
/// the class instead — so the hard "portable required" gate lives in the sync layer, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoIdentityClass {
    /// Machine-stable: derived from the repo's root commit, or an explicit `[index] repo_id` pin.
    /// The same repo resolves to the same id on every machine, so peer sync can replicate it.
    Portable,
    /// Deterministic on THIS clone but depth-dependent: a shallow clone whose history was cut, so
    /// the root commit is unreachable and the id is derived from the shallow BOUNDARY commits
    /// instead. Two clones of one repo at different depths derive DIFFERENT ids, so this id must
    /// never cross machines. The remedy is `git fetch --unshallow` (recovers the real root) or a
    /// pinned `repo_id`; until then indexing proceeds locally under the `local:` id.
    LocalOnly,
}

/// A repo's identity for the registry: the scoping key + a human label + its portability class.
#[derive(Debug, Clone)]
pub struct RepoIdentity {
    /// Machine-stable, content-derived (root-commit hash) unless pinned via `rag-rat.toml`, or a
    /// `local:`-prefixed boundary hash for a cut shallow clone. Never derived from the on-disk
    /// path.
    pub repo_id: String,
    /// The working-tree directory name — cosmetic only, never an identity input.
    pub display_name: String,
    /// Whether [`repo_id`](Self::repo_id) is portable across machines (peer sync eligible) or
    /// only stable on this clone (a cut shallow clone). Never an identity input; a
    /// scoping-neutral tag.
    pub class: RepoIdentityClass,
    /// For a [`LocalOnly`](RepoIdentityClass::LocalOnly) id, the SORTED shallow-boundary commit
    /// hashes the id was derived from; EMPTY for a `Portable` id (a full history or a pin has no
    /// boundary). `register_repo` persists these at LocalOnly registration so a later
    /// LocalOnly→Portable upgrade can PROVE the incoming deepened clone is the same repository —
    /// its HEAD must reach these boundary commits ([`boundary_reachable_from_head`]). Never an
    /// identity input; the id is already a hash OF this boundary.
    pub shallow_boundary: Vec<String>,
}

/// Why [`resolve_repo_identity`] could not derive a `repo_id`, split into the two classes its one
/// production caller (`IndexDatabase::adopt_repo_from_config`) MUST treat differently. Stringly
/// matching the message to tell them apart is not acceptable — the variant is the contract.
///
/// A cut shallow clone is DELIBERATELY not in here: since it derives a [`LocalOnly`] id rather than
/// failing, it is an `Ok` outcome, not an error. `Rejected` therefore covers only genuinely-invalid
/// inputs (a pinned reserved id, an unreadable / structurally-rootless history).
///
/// [`LocalOnly`]: RepoIdentityClass::LocalOnly
#[derive(Debug, thiserror::Error)]
pub enum RepoIdentityError {
    /// EXPECTED absence: the directory simply has no identity to derive yet — it is not a git
    /// repository, or it has an unborn HEAD (zero commits). This is the ordinary shape of a
    /// config-less / temp-dir / test index, so the caller falls back to the sole registered repo
    /// and leaves the DB single-repo and un-adopted (the pre-A3 behavior).
    #[error("{0}")]
    Absent(String),
    /// A user-actionable REJECTION that MUST surface, never silently fall back: a pinned reserved
    /// id, or an unreadable / root-less history (a walk failure, or a non-shallow graph with no
    /// reachable parentless commit). Falling back here would scope the DB to the placeholder and
    /// hide the real configuration problem, leaving every row unadopted under the legacy id.
    #[error("{0}")]
    Rejected(String),
}

impl RepoIdentityError {
    /// Whether this is the EXPECTED-absence class a config-less single-repo open may fall back on
    /// (not a git repo / unborn HEAD), as opposed to a [`Rejected`](Self::Rejected) config that
    /// must surface. The one predicate the registry adoption path branches on.
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent(_))
    }
}

/// Resolve a repo's identity from its working tree at `root`.
///
/// The default `repo_id` is the **lexicographically smallest root-commit hash** reachable from
/// HEAD (a [`Portable`](RepoIdentityClass::Portable) id). A linear history has exactly one root
/// (parentless) commit; a history stitched from several orphan branches has several, and choosing
/// the smallest by byte order makes the id deterministic across machines regardless of which root
/// the walk reaches first.
///
/// `override_id` (from `rag-rat.toml`'s `[index] repo_id`) wins outright when set and non-empty — a
/// `Portable` id by construction (the user chose a stable string). It is the escape hatch for forks
/// that must NOT share identity with their upstream, and for pinning an id on a repo whose root is
/// unreachable (an empty repo, or a shallow clone that cut history).
///
/// A shallow clone whose history was CUT (its root is unreachable) does NOT error: it derives a
/// deterministic [`LocalOnly`](RepoIdentityClass::LocalOnly) id from the sorted shallow-boundary
/// commits, logs one warning naming the future sync constraint, and proceeds. Blocking indexing on
/// a shallow checkout would break CI fixtures and `--depth 1` clones for no benefit — the id is
/// stable on this machine, only not across machines.
///
/// Errors — each with an actionable remedy — when no override was supplied and the id cannot be
/// derived, split by [`RepoIdentityError`] class: [`Absent`](RepoIdentityError::Absent) for the
/// directory not being a git repository or having no commits (unborn HEAD);
/// [`Rejected`](RepoIdentityError::Rejected) for a pinned reserved id or an unreadable / root-less
/// history.
pub fn resolve_repo_identity(
    root: &Path,
    override_id: Option<&str>,
) -> Result<RepoIdentity, RepoIdentityError> {
    let display_name =
        root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();

    if let Some(id) = override_id.map(str::trim).filter(|id| !id.is_empty()) {
        // The placeholder marker is reserved for a pre-adoption single-repo DB; pinning it would
        // degenerate registry adoption (see `register_repo`). Refuse with a remedy-naming error
        // rather than mint an identity that can never be adopted.
        if id == LEGACY_REPO_ID {
            return Err(RepoIdentityError::Rejected(reserved_repo_id_error()));
        }
        // The `local:` prefix is reserved for machine-derived shallow-clone ids: `register_repo`
        // keys its LocalOnly→Portable upgrade off an incumbent id carrying it, so a pinned
        // `local:`-prefixed id would be ambiguously treated as upgradeable. Refuse it with a
        // remedy.
        if id.starts_with(LOCAL_ONLY_ID_PREFIX) {
            return Err(RepoIdentityError::Rejected(reserved_local_prefix_error()));
        }
        // A pinned id is portable by construction — the user chose a stable string.
        return Ok(RepoIdentity {
            repo_id: id.to_string(),
            display_name,
            class: RepoIdentityClass::Portable,
            shallow_boundary: Vec::new(),
        });
    }

    let (repo_id, class, shallow_boundary) = derive_repo_id(root)?;
    if class == RepoIdentityClass::LocalOnly {
        // ONE warning per resolution, naming the future constraint and both remedies. The id is
        // usable NOW (single-repo indexing on this machine); the warning is about peer sync,
        // which will refuse a non-portable id.
        tracing::warn!(
            repo_id = %repo_id,
            root = %root.display(),
            "shallow clone: derived a machine-local repo identity because the root commit is \
             unreachable (history is cut). Indexing proceeds, but peer sync requires a portable \
             identity — run `git fetch --unshallow` to derive the stable root-commit id, or pin \
             `[index] repo_id` in rag-rat.toml."
        );
    }
    Ok(RepoIdentity { repo_id, display_name, class, shallow_boundary })
}

/// Whether `root` has a DERIVABLE repo identity at all — a non-empty `[index] repo_id` pin, or a
/// git repository with a born HEAD. The cheap existence probe behind the A7 default-database
/// resolution: an identity-LESS root (non-git dir, unborn `git init`) must stay on its per-root
/// legacy database rather than resolve to the machine-global store, where every such root would
/// pool under the shared `__unassigned__` placeholder scope (mutual visibility/overwrite) and an
/// unborn repo would strand its placeholder rows the moment its first commit mints a real id.
///
/// Deliberately NEVER walks history (`Config::load` runs on every command): it answers only
/// "would [`resolve_repo_identity`] return [`Absent`](RepoIdentityError::Absent)?" — the
/// Portable/LocalOnly split and Rejected-pin surfacing still happen at open time. A non-empty pin
/// counts as resolvable even when reserved (a Rejected pin must surface through the open's
/// actionable error, not silently divert the database path).
pub fn identity_is_resolvable(root: &Path, override_id: Option<&str>) -> bool {
    if override_id.map(str::trim).is_some_and(|id| !id.is_empty()) {
        return true;
    }
    let Ok(repo) = crate::repo_discover::discover_repo(root) else {
        return false;
    };
    repo.head_id().is_ok()
}

/// The remedy-naming error for pinning the reserved placeholder id: it marks a pre-adoption
/// single-repo DB, so adopting under it would rewrite the placeholder PK to itself and leave the DB
/// unadopted while reporting success (see `register_repo`).
fn reserved_repo_id_error() -> String {
    format!(
        "`{LEGACY_REPO_ID}` is a reserved repo_id (the pre-adoption placeholder marker) and \
         cannot be pinned via `[index] repo_id` in rag-rat.toml. Choose a different stable \
         string, or omit the override to derive the id from the root commit."
    )
}

/// The remedy-naming error for pinning a `local:`-prefixed id: the prefix is reserved for
/// machine-derived shallow-clone ids, and `register_repo` treats an incumbent id carrying it as
/// upgradeable to a portable id — pinning one would be ambiguous.
fn reserved_local_prefix_error() -> String {
    format!(
        "the `{LOCAL_ONLY_ID_PREFIX}` prefix is reserved for machine-derived shallow-clone repo \
         ids and cannot be pinned via `[index] repo_id` in rag-rat.toml. Choose a different \
         stable string, or omit the override to derive the id from the root commit."
    )
}

/// Derive the `repo_id` and its portability class from the commit graph at `root`.
///
/// The default id is the lexicographically smallest hash among the parentless commits reachable
/// from HEAD (a [`Portable`](RepoIdentityClass::Portable) id) — mirrors the git-history walk
/// (`index::git_history`): discover the repo, resolve HEAD, `rev_walk`, keep the commits whose
/// first parent is absent.
///
/// When NO parentless root is reachable — the exact signature of a shallow clone that CUT history
/// (gix reads parents from the commit object and does not apply shallow grafts, so the boundary
/// commit reports parents whose objects are absent, never a root, while the walk itself honors the
/// boundary and yields no true root) — it derives a [`LocalOnly`](RepoIdentityClass::LocalOnly) id
/// from the sorted shallow-boundary commits instead of failing. That id is stable across opens of
/// THIS clone but varies with clone depth, hence the non-portable class. A non-shallow graph with
/// no reachable root is genuinely corrupt and is [`Rejected`](RepoIdentityError::Rejected). A
/// shallow clone whose depth COVERS the whole history keeps its true root reachable and resolves
/// normally to a `Portable` id — hence the "no reachable root" signal rather than
/// `repo.is_shallow()` alone, which is also set for those fully-present clones.
fn derive_repo_id(
    root: &Path,
) -> Result<(String, RepoIdentityClass, Vec<String>), RepoIdentityError> {
    // Not a git repository at all → the EXPECTED-absence class (config-less temp dirs, tests).
    let repo = crate::repo_discover::discover_repo(root).map_err(|err| {
        RepoIdentityError::Absent(format!(
            "cannot derive a repo_id: {} is not a git repository ({err}). Run `git init`, or pin \
             `[index] repo_id = \"…\"` in rag-rat.toml.",
            root.display()
        ))
    })?;
    // Unborn HEAD (`git init` with no commits) has no history to derive an id from — also the
    // expected-absence class.
    let Ok(head) = repo.head_id() else {
        return Err(RepoIdentityError::Absent(empty_repo_error(root)));
    };
    let head_id = head.detach();

    // A history that EXISTS but cannot be read is not "no identity yet" — it must surface, so the
    // walk failures below are the Rejected class, never a silent fallback.
    let walk_failed = |err: &dyn std::fmt::Display| {
        RepoIdentityError::Rejected(format!(
            "cannot derive a repo_id: failed to walk the history at {}: {err}",
            root.display()
        ))
    };
    let mut roots: Vec<String> = Vec::new();
    for info in repo.rev_walk([head_id]).all().map_err(|err| walk_failed(&err))? {
        let info = info.map_err(|err| walk_failed(&err))?;
        let commit = repo.find_commit(info.id).map_err(|err| walk_failed(&err))?;
        // A root commit has no parents; `parent_ids().next().is_none()` is the same predicate the
        // history walk uses to detect roots. A cut shallow boundary reports (absent) parents, so it
        // is deliberately NOT counted here.
        if commit.parent_ids().next().is_none() {
            roots.push(info.id.to_hex().to_string());
        }
    }
    roots.sort();
    if let Some(smallest) = roots.into_iter().next() {
        return Ok((smallest, RepoIdentityClass::Portable, Vec::new()));
    }

    // No reachable parentless root. A cut shallow clone records its boundary in `.git/shallow`
    // (gix exposes it, already SORTED for bisecting); derive a deterministic LocalOnly id from it
    // rather than failing. A graph with neither a root NOR a shallow boundary is genuinely corrupt.
    match repo.shallow_commits() {
        Ok(Some(boundary)) => {
            // Carry the sorted boundary hashes alongside the id so `register_repo` can persist them
            // for the later upgrade-proof check; the id is a hash OF exactly these bytes.
            let hashes = boundary_hashes(&boundary);
            Ok((local_only_id_from_hashes(&hashes), RepoIdentityClass::LocalOnly, hashes))
        },
        Ok(None) => Err(RepoIdentityError::Rejected(format!(
            "repository at {} has no reachable root commit and is not a shallow clone (corrupt or \
             root-less history). Pin `[index] repo_id = \"…\"` in rag-rat.toml to index it anyway.",
            root.display()
        ))),
        Err(err) => Err(RepoIdentityError::Rejected(format!(
            "cannot derive a repo_id: failed to read the shallow boundary at {}: {err}",
            root.display()
        ))),
    }
}

/// The shallow-boundary commit hashes as sorted hex strings. gix keeps `.git/shallow` sorted (for
/// bisecting), so the order is deterministic across opens of the SAME clone.
fn boundary_hashes(boundary: &gix::shallow::Commits) -> Vec<String> {
    boundary.iter().map(|id| id.to_hex().to_string()).collect()
}

/// A deterministic `local:`-prefixed id hashed from the SORTED shallow-boundary commit hashes. Two
/// opens of the SAME clone hash the identical byte sequence and derive the same id — the
/// determinism the `LocalOnly` class promises. A DEEPER clone of the same repo has a different
/// boundary and thus a different id, which is exactly why the id is not portable across machines.
/// Kept byte-for-byte compatible with the pre-refactor derivation (sorted hashes joined by `\n`,
/// then sha256).
fn local_only_id_from_hashes(hashes: &[String]) -> String {
    let mut material = String::new();
    for hash in hashes {
        material.push_str(hash);
        material.push('\n');
    }
    format!("{LOCAL_ONLY_ID_PREFIX}{}", crate::hash::hex_sha256(material.as_bytes()))
}

/// Whether EVERY commit in `boundary` is reachable from the repository's HEAD at `root` — the PROOF
/// that a deepened clone is the same repository as a `LocalOnly` incumbent whose recorded shallow
/// boundary these hashes are. A `git fetch --unshallow` (or deeper fetch) keeps the old boundary
/// commits in history (they are real commits, now with their parents present), so they stay
/// ancestors of HEAD; an UNRELATED repo's HEAD reaches none of them. `register_repo` gates the
/// LocalOnly→Portable upgrade on this so it never re-points a local index onto a genuinely
/// different repo's id.
///
/// An empty `boundary` is never provable (returns `false`): the caller treats "no recorded
/// boundary" as no proof. Errors surface (the caller refuses on them) rather than silently
/// upgrading. Walks HEAD's ancestry once, short-circuiting as soon as every boundary commit is
/// found — mirrors [`derive_repo_id`]'s `rev_walk`.
pub fn boundary_reachable_from_head(root: &Path, boundary: &[String]) -> anyhow::Result<bool> {
    if boundary.is_empty() {
        return Ok(false);
    }
    let repo = crate::repo_discover::discover_repo(root).map_err(|err| {
        anyhow::anyhow!("cannot open the repository at {}: {err}", root.display())
    })?;
    let head = repo
        .head_id()
        .map_err(|err| anyhow::anyhow!("cannot resolve HEAD at {}: {err}", root.display()))?;
    let mut needed: std::collections::HashSet<&str> = boundary.iter().map(String::as_str).collect();
    for info in repo.rev_walk([head.detach()]).all()? {
        let info = info?;
        needed.remove(info.id.to_hex().to_string().as_str());
        if needed.is_empty() {
            return Ok(true);
        }
    }
    Ok(needed.is_empty())
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
        crate::test_git::run(root, args);
    }

    fn git_out(root: &Path, args: &[&str]) -> String {
        crate::test_git::output(root, args)
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
        assert_eq!(identity.class, RepoIdentityClass::Portable, "a full history is portable");
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
        assert_eq!(identity.class, RepoIdentityClass::Portable);
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
        assert_eq!(
            identity.class,
            RepoIdentityClass::Portable,
            "a pin is portable by construction"
        );
        assert_ne!(identity.repo_id, derived);

        // An empty/whitespace override falls through to the derived id.
        let blank = resolve_repo_identity(&root, Some("   ")).unwrap();
        assert_eq!(blank.repo_id, derived);
        assert_eq!(blank.class, RepoIdentityClass::Portable);
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
        assert!(!err.is_absent(), "a reserved pin is the Rejected class — it must propagate");
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

    /// The `local:` prefix is reserved for machine-derived shallow-clone ids: `register_repo` keys
    /// its LocalOnly→Portable upgrade off an incumbent id carrying it, so a pinned
    /// `local:`-prefixed id would be ambiguous. The resolver refuses it (the `Rejected` class),
    /// like the placeholder.
    #[test]
    fn override_with_the_reserved_local_prefix_is_refused() {
        let root = temp_root();
        init_repo(&root);
        git(&root, &["commit", "--allow-empty", "-q", "-m", "genesis"]);

        let err = resolve_repo_identity(&root, Some("local:deadbeef"))
            .expect_err("a `local:`-prefixed pin must be refused");
        assert!(
            !err.is_absent(),
            "a reserved-prefix pin is the Rejected class — it must propagate"
        );
        let msg = err.to_string();
        assert!(msg.contains("local:"), "error names the reserved prefix: {msg}");
        assert!(msg.contains("reserved"), "error explains it is reserved: {msg}");
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
    /// boundary commit records parents whose objects are absent. Rather than fail (which would
    /// block indexing a `--depth 1` checkout — CI fixtures, quick clones), it derives a
    /// DETERMINISTIC `local:`-prefixed [`LocalOnly`] id from the sorted shallow boundary and
    /// proceeds. The id is stable across opens of the SAME clone; the pin escape hatch still
    /// overrides it to `Portable`.
    #[test]
    fn shallow_clone_that_cuts_history_gets_a_deterministic_local_only_id() {
        let base = temp_root();
        let (shallow, origin_root) = shallow_clone(&base, 5, 1, "shallow");
        // Sanity: the fixture is really a shallow clone that cut history.
        assert!(crate::repo_discover::discover_repo(&shallow).unwrap().is_shallow());

        let identity = resolve_repo_identity(&shallow, None)
            .expect("a cut shallow clone must NOT fail — it derives a LocalOnly id");
        assert_eq!(
            identity.class,
            RepoIdentityClass::LocalOnly,
            "a cut shallow clone is LocalOnly"
        );
        assert!(
            identity.repo_id.starts_with("local:"),
            "a LocalOnly id is `local:`-prefixed, got {}",
            identity.repo_id
        );
        assert_ne!(identity.repo_id, origin_root, "the id is the boundary hash, not the real root");

        // DETERMINISTIC across two opens of the same clone (the `.git/shallow` boundary is stable).
        let again = resolve_repo_identity(&shallow, None).unwrap();
        assert_eq!(identity.repo_id, again.repo_id, "same clone → same LocalOnly id");

        // The pin escape hatch still overrides a shallow clone to a Portable id.
        let pinned = resolve_repo_identity(&shallow, Some("pinned-shallow")).unwrap();
        assert_eq!(pinned.repo_id, "pinned-shallow");
        assert_eq!(pinned.class, RepoIdentityClass::Portable, "a pin overrides to Portable");
        std::fs::remove_dir_all(&base).ok();
    }

    /// A clone flagged shallow whose `--depth` COVERS the whole history is NOT depth-dependent: the
    /// boundary commit IS the true root (all its ancestors are present), so identity resolves to
    /// the real root hash (a `Portable` id) and matches the origin. `is_shallow()` alone would
    /// wrongly treat this as LocalOnly; the "no reachable parentless root" signal correctly accepts
    /// it as portable.
    #[test]
    fn shallow_clone_covering_full_history_resolves_the_real_root() {
        let base = temp_root();
        // depth == commit count: git still writes `.git/shallow`, but nothing is actually cut.
        let (shallow, origin_root) = shallow_clone(&base, 5, 5, "shallow");
        assert!(
            crate::repo_discover::discover_repo(&shallow).unwrap().is_shallow(),
            "git flags a depth-exact clone shallow"
        );

        let identity = resolve_repo_identity(&shallow, None)
            .expect("full history present → the real root is reachable");
        assert_eq!(identity.repo_id, origin_root, "id is the real root, depth-independent");
        assert_eq!(identity.class, RepoIdentityClass::Portable, "a covered clone is portable");
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
        assert!(err.is_absent(), "an unborn HEAD is the expected-absence class (fallback-safe)");
        let msg = err.to_string();
        assert!(msg.contains("no commits"), "error names the cause: {msg}");
        assert!(msg.contains("repo_id"), "error names the pin remedy: {msg}");

        // A pin still lets an empty repo be registered (a Portable id).
        let pinned = resolve_repo_identity(&root, Some("pinned-empty")).unwrap();
        assert_eq!(pinned.repo_id, "pinned-empty");
        assert_eq!(pinned.class, RepoIdentityClass::Portable);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn non_git_dir_without_override_is_an_error() {
        let root = temp_root(); // created, but never `git init`ed
        let err = resolve_repo_identity(&root, None)
            .expect_err("a non-git directory has no identity to derive");
        assert!(err.is_absent(), "not-a-git-repo is the expected-absence class (fallback-safe)");
        // ...but an override lets a non-git dir resolve (a repo with no commits can still be
        // pinned).
        let identity = resolve_repo_identity(&root, Some("pinned")).unwrap();
        assert_eq!(identity.repo_id, "pinned");
        assert_eq!(identity.class, RepoIdentityClass::Portable);
        std::fs::remove_dir_all(&root).ok();
    }
}
