//! Path rendering and resolution shared across subsystems.

use std::path::{Path, PathBuf};

/// `std::fs::canonicalize`, minus the Windows `\\?\` verbatim prefix wherever the plain spelling
/// names the same file — THE canonicalization every subsystem resolves a filesystem path through.
/// Identical to `std::fs::canonicalize` on Unix.
///
/// Windows' `canonicalize` always returns a VERBATIM path (`\\?\C:\Users\…`), and that spelling is
/// not interchangeable with the ordinary one:
///  * `git worktree add \\?\C:\…\linked` fails outright — `fatal: could not create leading
///    directories of '//?/C:/…': Invalid argument` — so a `config.root` in verbatim form cannot
///    build a linked worktree at all;
///  * gix reports `workdir()` in the ORDINARY form (`gix-discover` runs `dunce::simplified` over
///    the directory it discovers from), so a verbatim root strips to nothing against it and the
///    worktree overlay scopes its refresh at the repo root instead of the config root.
///
/// Verbatim form is kept exactly where it is load-bearing (UNC shares, paths past `MAX_PATH`,
/// reserved DOS names such as `CON`, components with a trailing dot or space): those are the cases
/// where the plain spelling would resolve somewhere else, or nowhere. `dunce` owns that rule, and
/// using the SAME crate gix does is what makes the two sides comparable rather than merely similar.
///
/// Every filesystem canonicalization in the workspace goes through here; `clippy.toml` disallows
/// `Path::canonicalize` / `std::fs::canonicalize` so a new call site cannot reintroduce a verbatim
/// path (#1048). It takes `impl AsRef<Path>` so it is a drop-in for both halves of the
/// `std::fs::canonicalize` / `Path::canonicalize` pair it replaces.
pub fn canonicalize(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    dunce::canonicalize(path.as_ref())
}

/// The verbatim-prefix rule of [`canonicalize`] applied to a path we did NOT resolve ourselves — a
/// user-supplied argument, a repository path handed back by another library. Borrowing, no I/O, and
/// the identity on Unix.
pub fn simplified(path: &Path) -> &Path {
    dunce::simplified(path)
}

/// [`canonicalize`], falling back to the [`simplified`] original when the path cannot be resolved
/// (it does not exist, or is unreadable). The fallback is simplified rather than returned verbatim
/// so BOTH arms hold the no-verbatim invariant — a caller that compares the result against a
/// canonical path must not start matching only on the success arm.
pub fn canonicalize_or_simplified(path: &Path) -> PathBuf {
    canonicalize(path).unwrap_or_else(|_| simplified(path).to_path_buf())
}

/// The [`simplified`] rule applied to a path that was PERSISTED as a string — `None` when the
/// stored spelling is already the one this binary produces, `Some(plain)` when it is a verbatim
/// path that must be rekeyed to keep matching.
///
/// The rekey seam for stored identity. Values like a `worktree_id` scope key or a recorded
/// `repo_roots.root` are compared TEXTUALLY against a freshly-canonicalized path, so an index
/// written when [`canonicalize`] still answered `\\?\C:\…` stops matching the moment this binary
/// answers `C:\…` — the rows go out of scope and GC prunes them as a dead checkout. Rewriting the
/// stored string through the SAME rule production uses is what keeps the two comparable; a blind
/// `\\?\` strip would not, because [`simplified`] deliberately KEEPS the prefix where it is
/// load-bearing (UNC, >`MAX_PATH`, reserved DOS names) and this binary still produces it there.
///
/// `None` on Unix for every input, so a rekey pass over a Unix index is a strict no-op.
pub fn rekeyed_from_verbatim(stored: &str) -> Option<String> {
    let plain = simplified(Path::new(stored)).to_string_lossy();
    (plain != stored).then(|| plain.into_owned())
}

/// The canonical `/`-separated rendering used everywhere a path is persisted or compared against
/// the `files` table — Windows separators normalize so the same file hashes/joins identically on
/// every platform.
pub fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `path` with `.` dropped and `..` applied to the preceding component, TEXTUALLY — no filesystem
/// access, no symlink resolution. `None` when a `..` has no component left to consume, which means
/// the path resolves above the filesystem root.
///
/// Lexical rather than `canonicalize` because the callers ask a question about what a path SAYS,
/// not about what exists: config validation runs before anything is walked, and a compilation
/// database's entries routinely name files through `../..` that may no longer exist. Canonicalizing
/// would also make the answer depend on how a root was spelled — a checkout reached through a
/// symlink would start disagreeing with itself.
///
/// Collapsing before any prefix comparison is load-bearing: `Path::starts_with` compares
/// COMPONENTS, so the uncollapsed `<root>/../elsewhere` still "starts with" `<root>` and every
/// upward escape would pass a containment test.
pub fn lexically_normalized(path: &Path) -> Option<std::path::PathBuf> {
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {},
            // A `..` after a root/prefix component has nothing to pop, and anything else that pops
            // past the start is an escape this cannot represent.
            std::path::Component::ParentDir =>
                if !normalized.pop() {
                    return None;
                },
            other => normalized.push(other),
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `path` carries the Windows extended-length (`\\?\`) prefix — asked TEXTUALLY,
    /// because that is how `git` and gix see it.
    fn is_verbatim(path: &Path) -> bool {
        path.as_os_str().to_string_lossy().starts_with(r"\\?\")
    }

    /// The invariant the whole module exists for: an ordinary existing directory resolves to a
    /// NON-verbatim path, and that path still names the same directory. `std::fs::canonicalize`
    /// fails the first half on Windows for every path it is given (#1048).
    #[test]
    fn canonicalize_resolves_an_ordinary_directory_without_the_verbatim_prefix() {
        let dir = crate::test_scratch::ScratchDir::new("verbatim-probe");
        let canonical = canonicalize(dir.path()).unwrap();
        assert!(
            !is_verbatim(&canonical),
            "a canonicalized ordinary directory must not carry the verbatim prefix: {canonical:?}",
        );
        // Same directory under both spellings — the prefix strip resolved, it did not redirect.
        std::fs::write(canonical.join("probe"), "payload").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("probe")).unwrap(), "payload");
        // Idempotent: re-resolving the result is a fixed point.
        assert_eq!(canonicalize(&canonical).unwrap(), canonical);
    }

    /// [`simplified`] applies the same rule without touching the filesystem, and is the identity
    /// on a path that is already in the plain spelling.
    #[test]
    fn simplified_leaves_an_already_plain_path_alone() {
        let dir = crate::test_scratch::ScratchDir::new("simplified-probe");
        let canonical = canonicalize(dir.path()).unwrap();
        assert_eq!(simplified(&canonical), canonical.as_path());
        assert!(!is_verbatim(simplified(&canonical)));
    }

    /// A spelling this binary already produces is left ALONE — the rekey must be a no-op for every
    /// value that is not stale, or a migration would rewrite rows it has no business touching.
    /// Holds on both platforms: `C:\plain` is what Windows produces, and on Unix `simplified` is
    /// the identity for everything.
    #[test]
    fn rekeyed_from_verbatim_leaves_a_current_spelling_alone() {
        assert_eq!(rekeyed_from_verbatim(r"C:\plain\root"), None);
        assert_eq!(rekeyed_from_verbatim("/home/user/repo"), None);
        assert_eq!(rekeyed_from_verbatim(""), None);
    }

    /// On Unix NOTHING is rekeyed, including a string that merely LOOKS verbatim: a Unix index
    /// never held one, and rewriting a path that happens to start with those bytes would corrupt
    /// it. This is what makes the migration a strict no-op off Windows.
    #[cfg(unix)]
    #[test]
    fn rekeyed_from_verbatim_is_inert_on_unix() {
        assert_eq!(rekeyed_from_verbatim(r"\\?\C:\Users\dev\repo"), None);
    }

    /// The rekey itself: a stored verbatim path maps to the plain spelling `canonicalize` now
    /// answers, and the prefix is KEPT where it is load-bearing (a UNC share), because this binary
    /// still produces it there and rewriting would break the match it exists to preserve.
    #[cfg(windows)]
    #[test]
    fn rekeyed_from_verbatim_rewrites_only_the_droppable_prefix() {
        assert_eq!(
            rekeyed_from_verbatim(r"\\?\C:\Users\dev\repo").as_deref(),
            Some(r"C:\Users\dev\repo"),
        );
        // Load-bearing verbatim: `simplified` keeps it, so the stored value is already current.
        assert_eq!(rekeyed_from_verbatim(r"\\?\UNC\server\share\repo"), None);
        // Idempotent — re-running a rekey pass changes nothing the first one produced.
        assert_eq!(rekeyed_from_verbatim(r"C:\Users\dev\repo"), None);
    }

    /// The fallback arm must hold the invariant too: a path that cannot be resolved comes back
    /// SIMPLIFIED, not verbatim, so a caller comparing it against a canonical path does not start
    /// matching only when the directory happens to exist.
    #[test]
    fn canonicalize_or_simplified_falls_back_without_reintroducing_the_prefix() {
        let dir = crate::test_scratch::ScratchDir::new("fallback-probe");
        let absent = dir.path().join("not-created-yet");
        let resolved = canonicalize_or_simplified(&absent);
        assert!(!is_verbatim(&resolved), "the fallback must not be verbatim: {resolved:?}");
        assert_eq!(resolved, simplified(&absent));

        // And the success arm is plain `canonicalize`.
        std::fs::create_dir_all(&absent).unwrap();
        assert_eq!(canonicalize_or_simplified(&absent), canonicalize(&absent).unwrap());
    }
}
