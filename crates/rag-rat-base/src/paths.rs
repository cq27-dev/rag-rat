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
/// stored string through the same rule keeps the two comparable; a blind `\\?\` strip would not,
/// because the prefix is load-bearing where the plain spelling names something else (UNC shares,
/// paths past `MAX_PATH`, reserved DOS names), and a Windows binary still produces it there.
///
/// DECIDED TEXTUALLY, ON EVERY PLATFORM — this is the one place the verbatim rule is NOT delegated
/// to `dunce`. A stored string is data, not a path on this host: the store that carries Windows
/// spellings is not necessarily opened by a Windows binary (one repo directory reachable from both
/// a Windows and a WSL/container mount is one SQLite file), and `dunce::simplified` compiles to the
/// identity off Windows. Deferring to it would let whichever binary opened first record the rekey
/// migration as applied without performing it, after which the ladder — forward-only — never
/// revisits it and the Windows binary keeps the spellings that get its rows collected as a dead
/// checkout. Nothing here touches the filesystem: droppability is a property of the string.
/// `windows_verbatim_droppability_matches_dunce` pins the rule against `dunce::simplified` on the
/// Windows leg of CI, so the two cannot drift.
///
/// Only the verbatim DISK shape (`\\?\C:\…`) is ever rewritten. No rag-rat writes a value in that
/// shape today on any platform — post-fix Windows records `C:\…`, Unix records `/…` — so a Unix
/// directory deliberately named to look like one is out of scope rather than defended against.
pub fn rekeyed_from_verbatim(stored: &str) -> Option<String> {
    let plain = stored.strip_prefix(VERBATIM_PREFIX)?;
    verbatim_disk_prefix_is_droppable(stored, plain).then(|| plain.to_string())
}

/// The Windows extended-length prefix a pre-fix `canonicalize` put on every path it resolved.
const VERBATIM_PREFIX: &str = r"\\?\";

/// Whether `plain` — `stored` minus its [`VERBATIM_PREFIX`] — names the same file as `stored`.
///
/// `stored` is passed alongside because the length limit applies to the WHOLE verbatim path, which
/// is what the plain spelling would have to fit under.
fn verbatim_disk_prefix_is_droppable(stored: &str, plain: &str) -> bool {
    // A verbatim DISK path is the only form whose prefix is droppable: `\\?\UNC\server\share` and
    // the raw device forms name things the drive-letter spelling cannot reach at all.
    let mut prefix = plain.chars();
    match (prefix.next(), prefix.next()) {
        (Some(drive), Some(':')) if drive.is_ascii_alphabetic() => {},
        _ => return false,
    }
    // Everything after `C:` is the root separator plus the components under it. A bare `C:` (no
    // root) and `C:\` (root, no components) are both already whole paths.
    if let Some(under_root) = plain.get(2..).filter(|rest| !rest.is_empty()) {
        let Some(components) = under_root.strip_prefix('\\') else { return false };
        if !components.is_empty()
            && !components.split('\\').all(component_is_reachable_without_the_prefix)
        {
            return false;
        }
    }
    // Past the legacy limit the plain spelling is not addressable through the ANSI APIs, so the
    // prefix is what makes the path usable. Byte length is checked first because it is the cheap
    // over-estimate: only a path that fails it is worth counting UTF-16 units for.
    stored.len() <= MAX_PLAIN_PATH_UNITS || windows_utf16_len(stored) <= MAX_PLAIN_PATH_UNITS
}

/// Whether one path component resolves to itself under the plain spelling.
///
/// The DOS layer the plain spelling goes through trims trailing dots and spaces, reserves a handful
/// of device names, and rejects a set of characters outright — a component that trips any of those
/// resolves somewhere else, or nowhere, once the prefix is gone.
fn component_is_reachable_without_the_prefix(component: &str) -> bool {
    if component.is_empty() {
        return false;
    }
    if component.len() > MAX_COMPONENT_UNITS && windows_utf16_len(component) > MAX_COMPONENT_UNITS {
        return false;
    }
    if component.bytes().any(|b| {
        matches!(b, 0..=31 | b'<' | b'>' | b':' | b'"' | b'/' | b'\\' | b'|' | b'?' | b'*')
    }) {
        return false;
    }
    // Trailing dots and spaces are trimmed by the DOS layer, so the plain spelling names a
    // different file. This also rejects `.` and `..`, which a verbatim path takes literally.
    if component.ends_with('.') || component.ends_with(' ') {
        return false;
    }
    !names_a_reserved_dos_device(component)
}

/// Whether `component` is one of the reserved DOS device names — `CON`, `NUL`, `COM1`, … — under
/// which the plain spelling opens the DEVICE rather than the file.
///
/// The reservation applies to the STEM (`con.txt` is `CON`) after trailing dots and spaces are
/// trimmed (`con. . ` is `CON`).
fn names_a_reserved_dos_device(component: &str) -> bool {
    const RESERVED: [&str; 22] = [
        "AUX", "NUL", "PRN", "CON", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    // `Path::file_stem`'s rule: everything before the LAST dot, unless the only dot is a leading
    // one (a dotfile is all stem).
    let stem = match component.rfind('.') {
        Some(0) | None => component,
        Some(dot) => &component[..dot],
    };
    let trimmed = stem.trim_end_matches([' ', '.']);
    // Every reserved name is ASCII and at most 4 bytes, so the length test rejects the long
    // majority before any comparison — and keeps a multi-byte suffix (`CON。`) from matching.
    trimmed.len() <= 4 && RESERVED.iter().any(|name| trimmed.eq_ignore_ascii_case(name))
}

/// The length of `s` in UTF-16 code units — the unit Windows measures its path limits in, counted
/// from the UTF-8 without encoding.
fn windows_utf16_len(s: &str) -> usize {
    s.chars().map(|c| if (c as u32) <= 0xFFFF { 1 } else { 2 }).sum()
}

/// The longest plain-spelled path the ANSI APIs accept, and the longest single component.
const MAX_PLAIN_PATH_UNITS: usize = 260;
const MAX_COMPONENT_UNITS: usize = 255;

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

    /// The corpus the rekey rule is pinned on, as `(stored, rekeyed)` pairs. `None` means the
    /// stored spelling is one a current binary still produces, so the value is already correct and
    /// must be left alone.
    ///
    /// Shared by the platform-independent test below and by the Windows-only agreement test, so the
    /// rule and `dunce` are compared on exactly the cases the rule is specified against rather than
    /// on two separately-remembered lists.
    const REKEY_CORPUS: &[(&str, Option<&str>)] = &[
        // The ordinary case, and the whole point: a checkout root a pre-fix binary resolved.
        (r"\\?\C:\Users\dev\repo", Some(r"C:\Users\dev\repo")),
        (r"\\?\c:\lower\drive", Some(r"c:\lower\drive")),
        (r"\\?\C:\", Some(r"C:\")),
        (r"\\?\C:", Some("C:")),
        // Already current — a rekey pass must be idempotent and must not touch a Unix path.
        (r"C:\Users\dev\repo", None),
        ("/home/user/repo", None),
        ("", None),
        // Load-bearing verbatim: the plain spelling names something else, or nothing.
        (r"\\?\UNC\server\share\repo", None),
        (r"\\?\Volume{4c1b02c1-d990-11dc-99ae-806e6f6e6963}\repo", None),
        // Reserved DOS device names, including through a stem and through trimmed dots/spaces.
        (r"\\?\C:\CON\repo", None),
        (r"\\?\C:\repo\com4.txt", None),
        (r"\\?\C:\repo\con . .txt", None),
        // Not reserved, though it looks close.
        (r"\\?\C:\repo\COM77", Some(r"C:\repo\COM77")),
        (r"\\?\C:\repo\not.CON", Some(r"C:\repo\not.CON")),
        // Trailing dot / space: the DOS layer trims both, so the plain spelling is a different
        // file. `..` is taken literally in a verbatim path and is caught by the same rule.
        (r"\\?\C:\repo\trailing.", None),
        (r"\\?\C:\repo\trailing ", None),
        (r"\\?\C:\repo\..\sibling", None),
        // A character the plain spelling cannot carry.
        (r"\\?\C:\repo\pipe|name", None),
    ];

    /// The rekey rule, on every platform. It decides a property of a STORED STRING, so it cannot be
    /// allowed to depend on the host that happens to be reading it: a store carrying Windows
    /// spellings is reachable from a non-Windows binary, and a rekey that no-ops there would let
    /// the migration record itself as applied without converting anything.
    #[test]
    fn rekeyed_from_verbatim_rewrites_only_the_droppable_prefix() {
        for (stored, expected) in REKEY_CORPUS {
            assert_eq!(
                rekeyed_from_verbatim(stored).as_deref(),
                *expected,
                "the rekey rule must not depend on the host reading the store: {stored:?}",
            );
        }
    }

    /// A path past the legacy limit keeps its prefix — that is the case the prefix exists for — and
    /// one just under it does not. Built rather than spelled out because the boundary is 260 units.
    ///
    /// Both paths are made of 100-unit components, so the WHOLE-PATH limit is what separates them:
    /// a single 300-unit component would be rejected by the 255-unit per-component rule instead,
    /// and the test would pass without the length check existing at all.
    #[test]
    fn rekeyed_from_verbatim_keeps_the_prefix_past_the_legacy_length_limit() {
        let component = "a".repeat(100);
        let long = format!(r"\\?\C:\{component}\{component}\{component}");
        assert!(long.len() > 260, "the fixture must actually exceed the limit: {}", long.len());
        assert_eq!(rekeyed_from_verbatim(&long), None, "a >MAX_PATH path needs its prefix");

        let short = format!(r"\\?\C:\{component}\{component}");
        assert!(short.len() <= 260, "the fixture must stay inside the limit: {}", short.len());
        assert_eq!(
            rekeyed_from_verbatim(&short).as_deref(),
            Some(&short[4..]),
            "a path inside the limit is reachable without the prefix",
        );
    }

    /// The rule is a reimplementation of `dunce`'s — the crate `simplified` delegates to, and the
    /// one gix resolves its `workdir()` through — so on Windows the two must agree exactly. This is
    /// what makes the platform-independent copy safe: if the rule ever drifts from the authority,
    /// the Windows leg of CI says so instead of the drift surfacing as rows rekeyed to a spelling
    /// production does not produce.
    #[cfg(windows)]
    #[test]
    fn windows_verbatim_droppability_matches_dunce() {
        for (stored, _) in REKEY_CORPUS {
            let authority = simplified(Path::new(stored)).to_string_lossy();
            let ours = rekeyed_from_verbatim(stored);
            assert_eq!(
                ours.as_deref(),
                (authority != *stored).then_some(authority.as_ref()),
                "the rekey rule disagrees with dunce about {stored:?}",
            );
        }
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
