//! Crate-aware import scope (#61, Project B). A reference to a name imported from an EXTERNAL
//! dependency crate must not bind to a local same-named symbol — the `use` says where the name
//! comes from. We tell a LOCAL workspace crate root (keep binding — e.g. cross-crate references
//! into the `cargo` crate, which the heuristic resolves correctly) from an external dependency root
//! (`url`, `serde_core`, `std`, … — don't bind locally) by parsing the corpus's Cargo.toml
//! manifests for the set of local crate names. Rust/Cargo-specific by construction: a corpus with
//! no manifests yields an empty set and the scope suppresses nothing (fail open).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

/// Load the persisted local-crate-root set — newline-joined under `local_crate_roots` in
/// `index_meta`, written at rebuild by [`local_crate_roots`]. Empty when absent (a non-Cargo corpus
/// or a pre-V021 index), which makes [`ImportScope`] suppress nothing.
pub(crate) fn load_local_roots(conn: &Connection) -> HashSet<String> {
    conn.query_row("SELECT value FROM index_meta WHERE key = 'local_crate_roots'", [], |row| {
        row.get::<_, String>(0)
    })
    .optional()
    .ok()
    .flatten()
    .map(|value| value.lines().filter(|line| !line.is_empty()).map(str::to_string).collect())
    .unwrap_or_default()
}

/// The set of LOCAL crate roots in the corpus: the underscore-normalized crate name of every
/// Cargo.toml package found under `root` (respecting .gitignore, so `target/` vendored manifests
/// are skipped). A `use <root>::…` whose `<root>` is in this set — or is `crate`/`self`/`super` —
/// names a local item; any other root names an external dependency.
pub(crate) fn local_crate_roots(root: &Path) -> HashSet<String> {
    let mut roots = HashSet::new();
    // `parents(false)`: honor the indexed root's OWN .gitignore (skip its `target/` vendored
    // manifests) but NOT ancestor gitignores above it — otherwise a root that happens to live under
    // some ancestor's ignored path (e.g. the bench corpus under the repo's `target/`) is treated as
    // wholly ignored and the walk yields nothing.
    for entry in ignore::WalkBuilder::new(root).parents(false).build().flatten() {
        if entry.path().file_name().and_then(|name| name.to_str()) != Some("Cargo.toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        let Ok(value) = toml::from_str::<toml::Value>(&text) else { continue };
        if let Some(name) = crate_root_name(&value) {
            roots.insert(name);
        }
    }
    roots
}

/// The crate root identifier used in `use` paths: `[lib].name` if set, else the package name with
/// `-` normalized to `_` (Cargo's crate-name rule: `cargo-credential` → `cargo_credential`).
fn crate_root_name(manifest: &toml::Value) -> Option<String> {
    if let Some(lib_name) =
        manifest.get("lib").and_then(|lib| lib.get("name")).and_then(|name| name.as_str())
    {
        return Some(lib_name.replace('-', "_"));
    }
    let package = manifest.get("package")?.get("name")?.as_str()?;
    Some(package.replace('-', "_"))
}

/// The crate root of a `use` statement: its first path segment (`use cargo::core::…` → `cargo`,
/// `pub use crate::x` → `crate`). `None` for a glob/unparseable form (we then suppress nothing).
pub(crate) fn use_root(use_text: &str) -> Option<&str> {
    let trimmed = use_text.trim_start();
    let trimmed = trimmed.strip_prefix("pub ").map(str::trim_start).unwrap_or(trimmed);
    let rest = trimmed.strip_prefix("use ")?.trim_start();
    rest.split([':', ' ', '{', ';', '*']).next().filter(|segment| !segment.is_empty())
}

/// Per-file map of imported leaf name → the crate root it was imported from, plus the local-crate
/// set, so resolution can tell an external-dependency import from a local one.
#[derive(Default)]
pub(crate) struct ImportScope {
    local_roots: HashSet<String>,
    by_file: HashMap<i64, HashMap<String, String>>,
}

impl ImportScope {
    pub(crate) fn new(local_roots: HashSet<String>) -> Self {
        Self { local_roots, by_file: HashMap::new() }
    }

    /// Record one `use <root>::…` binding for a file: the bound leaf name → its crate root.
    pub(crate) fn add_import(&mut self, file_id: i64, leaf: &str, root: &str) {
        self.by_file.entry(file_id).or_default().insert(leaf.to_string(), root.to_string());
    }

    /// Whether `name` in `file_id` was imported from an EXTERNAL dependency crate — not a local
    /// workspace crate, not `crate`/`self`/`super`. When true, the name denotes that dependency's
    /// item and must NOT bind to a local same-named symbol. Fails OPEN: with no local-crate set
    /// (non-Cargo corpus / manifest scan found nothing), nothing is ever suppressed.
    pub(crate) fn is_external_import(&self, file_id: i64, name: &str) -> bool {
        if self.local_roots.is_empty() {
            return false;
        }
        let Some(root) = self.by_file.get(&file_id).and_then(|names| names.get(name)) else {
            return false;
        };
        !self.local_roots.contains(root) && !matches!(root.as_str(), "crate" | "self" | "super")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_root_extracts_first_segment() {
        assert_eq!(use_root("use url::Url;"), Some("url"));
        assert_eq!(use_root("use cargo::core::Workspace;"), Some("cargo"));
        assert_eq!(use_root("pub use crate::a::B;"), Some("crate"));
        assert_eq!(use_root("use std::path::{Path, PathBuf};"), Some("std"));
        assert_eq!(use_root("use self::inner::X;"), Some("self"));
    }

    #[test]
    fn external_import_distinguishes_local_from_dependency() {
        let mut scope = ImportScope::new(HashSet::from(["cargo".to_string()]));
        scope.add_import(1, "Url", "url"); // external dep
        scope.add_import(1, "Workspace", "cargo"); // local workspace crate
        scope.add_import(1, "Write", "std"); // std = external
        scope.add_import(1, "Helper", "crate"); // local (crate-relative)
        assert!(scope.is_external_import(1, "Url"), "url is an external dep");
        assert!(scope.is_external_import(1, "Write"), "std is external");
        assert!(!scope.is_external_import(1, "Workspace"), "cargo is a local workspace crate");
        assert!(!scope.is_external_import(1, "Helper"), "crate-rooted is local");
        assert!(
            !scope.is_external_import(1, "Unimported"),
            "an unimported name is never suppressed"
        );
        assert!(!scope.is_external_import(2, "Url"), "scoped per file");
    }

    #[test]
    fn local_crate_roots_scans_workspace_manifests() {
        let dir = std::env::temp_dir().join(format!("rr-crate-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("crates/foo-bar/src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/*\"]\n[package]\nname=\"cargo\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("crates/foo-bar/Cargo.toml"), "[package]\nname=\"foo-bar\"\n")
            .unwrap();
        let roots = local_crate_roots(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(roots.contains("cargo"), "top package; got {roots:?}");
        assert!(roots.contains("foo_bar"), "hyphen→underscore normalized; got {roots:?}");
    }

    #[test]
    fn empty_local_set_fails_open() {
        let mut scope = ImportScope::new(HashSet::new());
        scope.add_import(1, "Url", "url");
        assert!(!scope.is_external_import(1, "Url"), "no manifests → suppress nothing");
    }
}
