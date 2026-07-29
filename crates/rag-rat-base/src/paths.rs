//! Path rendering shared across subsystems.

use std::path::Path;

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
