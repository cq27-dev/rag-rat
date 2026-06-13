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
        // Parse with the serde entry point (`toml::from_str`), NOT `text.parse::<toml::Value>()`:
        // `toml::Value`'s `FromStr` compiles but does not parse a full Cargo.toml document — it
        // returns Err on a valid manifest, silently emptying the local set.
        let Ok(manifest) = toml::from_str::<toml::Value>(&text) else { continue };
        let manifest_dir = entry.path().parent();
        if let Some(name) = lib_crate_root_name(&manifest, manifest_dir) {
            roots.insert(name);
        }
        // Renamed / path-dependency aliases: `local_alias = { path = "…", package = "real" }` lets
        // the import KEY (`use local_alias::…`) differ from the package's canonical crate name, so
        // the key must itself count as a local root or the bare reference OVER-suppresses (a
        // dropped LOCAL bind — the harmful direction, #97 item 2). The key is the crate
        // root written in source, so it needs no `-`→`_` normalization (a `use` path can't
        // contain `-`).
        collect_path_dependency_aliases(&manifest, &mut roots);
    }
    roots
}

/// The crate root identifier used in `use` paths when this manifest defines an importable LIBRARY:
/// `[lib].name` if the table sets one, else the package name (`-`→`_`, Cargo's crate-name rule:
/// `cargo-credential` → `cargo_credential`). Returns `None` for a BIN-ONLY member (no `[lib]` table
/// and no autodiscovered `src/lib.rs`): such a package is not importable, so contributing its name
/// as a root would wrongly mark a same-named EXTERNAL dependency as local and skip suppression
/// (#97 item 3). The `src/lib.rs` probe mirrors Cargo's lib autodiscovery and is resolved relative
/// to the manifest directory.
fn lib_crate_root_name(manifest: &toml::Value, manifest_dir: Option<&Path>) -> Option<String> {
    let lib = manifest.get("lib");
    if let Some(lib_name) = lib.and_then(|lib| lib.get("name")).and_then(|name| name.as_str()) {
        return Some(lib_name.replace('-', "_"));
    }
    let has_lib_target =
        lib.is_some() || manifest_dir.is_some_and(|dir| dir.join("src").join("lib.rs").is_file());
    if !has_lib_target {
        return None;
    }
    let package = manifest.get("package")?.get("name")?.as_str()?;
    Some(package.replace('-', "_"))
}

/// Add every `[dependencies]` / `[workspace.dependencies]` KEY whose value carries a `path` key to
/// `roots` — these are local path dependencies whose import name is the KEY (possibly renamed via
/// the `package` key), so the bare-reference scope must treat the key as a local crate root (#97
/// item 2). A string dependency (`serde = "1"`) or a non-`path` table dependency is external and
/// contributes nothing.
fn collect_path_dependency_aliases(manifest: &toml::Value, roots: &mut HashSet<String>) {
    let deps_tables = [
        manifest.get("dependencies"),
        manifest.get("workspace").and_then(|workspace| workspace.get("dependencies")),
    ];
    for table in deps_tables.into_iter().flatten().filter_map(toml::Value::as_table) {
        for (key, spec) in table {
            if spec.get("path").is_some() {
                roots.insert(key.clone());
            }
        }
    }
}

/// Parse a Rust `use` statement into its crate root and the leaf NAMES it actually binds into
/// scope — the only names a bare reference can resolve through. The imports edge stream enumerates
/// every identifier under a `use` path (`use std::path::{Path, PathBuf}` emits `std::path`, `Path`,
/// `PathBuf`), so building the scope from those over-records the path prefix `path` as a binding
/// and then wrongly suppresses an unrelated local `path`. Parsing the statement records only the
/// real bindings.
///
/// Handles restricted visibility (`pub(crate) use …`, `pub(in path) use …`), brace groups
/// (including nesting), `as` aliases (the alias is the bound name), and `self` in a group (binds
/// the parent segment). Returns `None` for a non-`use`/unparseable form; a glob (`*`) contributes
/// no specific binding (the names it brings in are unknowable, so it fails open — suppresses
/// nothing).
pub(crate) fn parse_use(use_text: &str) -> Option<(String, Vec<String>)> {
    let rest = strip_use_visibility(use_text.trim())?;
    let tree = rest.strip_suffix(';').unwrap_or(rest).trim();
    // Drop a leading path separator (`use ::external::X;`) so the root is `external`, not empty.
    let tree = tree.strip_prefix("::").unwrap_or(tree);
    // First `::`-segment, then its first whitespace token — strips a brace-free root alias
    // (`use my_crate as alias;` → root `my_crate`, not `my_crate as alias`).
    let root = tree
        .split("::")
        .next()
        .and_then(|seg| seg.split_whitespace().next())
        .filter(|seg| !seg.is_empty())?;
    let mut leaves = Vec::new();
    collect_use_leaves(tree, &mut leaves);
    Some((root.to_string(), leaves))
}

/// Strip an optional leading visibility modifier and the `use ` keyword, returning the use TREE
/// (everything after `use `). `None` when the statement isn't a `use` (e.g. a `mod foo;` Imports
/// edge), so the caller records nothing.
fn strip_use_visibility(s: &str) -> Option<&str> {
    let s = s.trim_start();
    // `pub use …`, `pub(crate) use …`, `pub(in path) use …`. A bare `pub` must be followed by a
    // boundary (whitespace or `(`) so we don't eat the `pub` prefix of an unrelated identifier.
    let s = match s.strip_prefix("pub") {
        Some(after) if after.starts_with('(') =>
            after.split_once(')').map(|(_, tail)| tail).unwrap_or(after).trim_start(),
        Some(after) if after.starts_with(char::is_whitespace) => after.trim_start(),
        _ => s,
    };
    s.strip_prefix("use ").map(str::trim_start)
}

/// The bound leaf names of a use (sub)tree, appended to `out`. A tree is either a single path
/// (`a::b::C`, optionally `… as Alias`) or a path with a brace group (`a::b::{…}`); recurse into
/// the group, which may itself nest.
fn collect_use_leaves(tree: &str, out: &mut Vec<String>) {
    let tree = tree.trim();
    match tree.find('{') {
        None =>
            if let Some(leaf) = single_use_binding(tree) {
                out.push(leaf);
            },
        Some(open) => {
            let prefix = tree[..open].trim_end().trim_end_matches(':');
            for entry in split_top_level(brace_inner(&tree[open..])) {
                let entry = entry.trim();
                if entry == "self" {
                    // `a::b::{self}` binds the parent segment `b`.
                    if let Some(parent) = prefix.rsplit("::").next().filter(|seg| !seg.is_empty()) {
                        out.push(parent.to_string());
                    }
                } else if !entry.is_empty() {
                    collect_use_leaves(entry, out);
                }
            }
        },
    }
}

/// The bound name of a single (brace-free) use path: the alias after `as`, else the last `::`
/// segment. A glob (`*`) or a bare `self`/empty tail binds nothing here (the caller fails open).
fn single_use_binding(path: &str) -> Option<String> {
    if let Some((_, alias)) = path.split_once(" as ") {
        let alias = alias.trim();
        return (!alias.is_empty() && alias != "_").then(|| alias.to_string());
    }
    match path.rsplit("::").next().map(str::trim) {
        Some("" | "*" | "self") | None => None,
        Some(name) => Some(name.to_string()),
    }
}

/// The content between the first `{` and its matching `}` (or to end-of-string if the evidence was
/// truncated). `from_brace` must start at the `{`.
fn brace_inner(from_brace: &str) -> &str {
    let mut depth = 0u32;
    for (i, b) in from_brace.bytes().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &from_brace[1..i];
                }
            },
            _ => {},
        }
    }
    &from_brace[1..]
}

/// Split a brace group's interior on top-level commas — commas inside a nested `{…}` don't split.
fn split_top_level(inner: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0u32;
    let mut start = 0;
    for (i, b) in inner.bytes().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(&inner[start..i]);
                start = i + 1;
            },
            _ => {},
        }
    }
    parts.push(&inner[start..]);
    parts
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

    /// Record a `use` statement's bindings for `file_id`: every leaf name it brings into scope →
    /// the crate root it came from. Idempotent — the same evidence text arrives once per identifier
    /// in the imports edge stream, and re-recording the same (leaf → root) is a no-op.
    pub(crate) fn add_use(&mut self, file_id: i64, use_text: &str) {
        let Some((root, leaves)) = parse_use(use_text) else { return };
        let file = self.by_file.entry(file_id).or_default();
        for leaf in leaves {
            file.insert(leaf, root.clone());
        }
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

    /// Whether a path-qualified reference's RECEIVER/root names an external import — `Url::parse`
    /// (target_qualified_name `Url::parse`) where `Url` was `use`d from the external `url` crate.
    /// The leaf `parse` itself isn't imported, so [`is_external_import`] on the callee name misses
    /// it; the new scope-path lookup would otherwise bind `Url::parse` to an in-repo `Url::parse`.
    ///
    /// Gated on a TYPE-LIKE (uppercase) head. `target_qualified_name` rewrites `.`→`::`
    /// (helpers::target_qualified_name), so a value-receiver method call `config.build()` arrives
    /// here as `config::build` and is INDISTINGUISHABLE from a path call by punctuation alone.
    /// Suppressing it would drop a valid local method call whenever the receiver's name happens to
    /// match an imported leaf. The realistic mis-bind this guards against is a PascalCase type
    /// collision (`Url::parse` → a same-named local `impl Url`); module/value heads are snake_case,
    /// so the uppercase gate keeps the type case and leaves lowercase heads to fall through
    /// (fail open — a missed suppression, never a dropped local bind).
    pub(crate) fn is_external_qualified_root(
        &self,
        file_id: i64,
        target_qualified_name: Option<&str>,
    ) -> bool {
        target_qualified_name.and_then(|qualified| qualified.split_once("::")).is_some_and(
            |(root, _)| {
                root.starts_with(char::is_uppercase) && self.is_external_import(file_id, root)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: `(root, sorted leaves)` so the assertions don't depend on traversal order.
    fn parsed(use_text: &str) -> Option<(String, Vec<String>)> {
        parse_use(use_text).map(|(root, mut leaves)| {
            leaves.sort();
            (root, leaves)
        })
    }

    #[test]
    fn parse_use_extracts_root_and_only_bound_leaves() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(parsed("use url::Url;"), Some(("url".into(), s(&["Url"]))));
        assert_eq!(
            parsed("use cargo::core::Workspace;"),
            Some(("cargo".into(), s(&["Workspace"])))
        );
        // The braced form: the path PREFIX `path` is NOT a binding — only `Path`/`PathBuf` are.
        assert_eq!(
            parsed("use std::path::{Path, PathBuf};"),
            Some(("std".into(), s(&["Path", "PathBuf"])))
        );
        // `use std::path;` DOES bind the module `path` (no braces, last segment is the binding).
        assert_eq!(parsed("use std::path;"), Some(("std".into(), s(&["path"]))));
        // Restricted visibility must still parse (was dropped by the old `pub ` strip).
        assert_eq!(parsed("pub(crate) use url::Url;"), Some(("url".into(), s(&["Url"]))));
        assert_eq!(parsed("pub(super) use a::B;"), Some(("a".into(), s(&["B"]))));
        assert_eq!(parsed("pub(in crate::x) use a::B;"), Some(("a".into(), s(&["B"]))));
        assert_eq!(parsed("pub use crate::a::B;"), Some(("crate".into(), s(&["B"]))));
        // `as` alias: the alias is the bound name.
        assert_eq!(parsed("use foo::Bar as Baz;"), Some(("foo".into(), s(&["Baz"]))));
        // Brace-free ROOT alias: the root is the pre-`as` crate, the leaf is the alias.
        assert_eq!(parsed("use my_crate as local;"), Some(("my_crate".into(), s(&["local"]))));
        assert_eq!(parsed("use foo::{Bar as Baz, Qux};"), Some(("foo".into(), s(&["Baz", "Qux"]))));
        // `self` in a group binds the parent segment.
        assert_eq!(parsed("use a::b::{self, C};"), Some(("a".into(), s(&["C", "b"]))));
        // Nested groups.
        assert_eq!(parsed("use a::{b::{C, D}, E};"), Some(("a".into(), s(&["C", "D", "E"]))));
        // Glob binds no specific name (fail open).
        assert_eq!(parse_use("use foo::*;"), Some(("foo".to_string(), Vec::new())));
        // Leading `::` (absolute path): root is the real crate, not empty.
        assert_eq!(parsed("use ::external::X;"), Some(("external".into(), s(&["X"]))));
        // Not a `use` statement.
        assert_eq!(parse_use("mod foo;"), None);
    }

    #[test]
    fn external_import_distinguishes_local_from_dependency() {
        let mut scope = ImportScope::new(HashSet::from(["cargo".to_string()]));
        scope.add_use(1, "use url::Url;"); // external dep
        scope.add_use(1, "use cargo::core::Workspace;"); // local workspace crate
        scope.add_use(1, "use std::path::{Path, PathBuf};"); // std = external
        scope.add_use(1, "use crate::a::Helper;"); // local (crate-relative)
        assert!(scope.is_external_import(1, "Url"), "url is an external dep");
        assert!(scope.is_external_import(1, "Path"), "std is external");
        assert!(!scope.is_external_import(1, "Workspace"), "cargo is a local workspace crate");
        assert!(!scope.is_external_import(1, "Helper"), "crate-rooted is local");
        assert!(
            !scope.is_external_import(1, "path"),
            "the `std::path` PREFIX is not a binding — a local `path` must stay resolvable"
        );
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
        // Both members carry a `src/lib.rs` so each is an importable LIBRARY (autodiscovery) and
        // contributes its package name as a root (#97 item 3 gates this on a lib target).
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("crates/foo-bar/src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/*\"]\n[package]\nname=\"cargo\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join("crates/foo-bar/Cargo.toml"), "[package]\nname=\"foo-bar\"\n")
            .unwrap();
        std::fs::write(dir.join("crates/foo-bar/src/lib.rs"), "").unwrap();
        let roots = local_crate_roots(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(roots.contains("cargo"), "top package; got {roots:?}");
        assert!(roots.contains("foo_bar"), "hyphen→underscore normalized; got {roots:?}");
    }

    /// #97 item 2: a renamed / path dependency (`local_alias = { path = …, package = "real" }`)
    /// imports under its KEY (`use local_alias::…`), which differs from the package's canonical
    /// crate name. The key must be a local root or a bare reference through it OVER-suppresses (a
    /// dropped LOCAL bind). String/non-path table deps are external and contribute nothing.
    #[test]
    fn local_crate_roots_recognizes_path_dependency_aliases() {
        let dir = std::env::temp_dir().join(format!("rr-path-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname=\"app\"\n[dependencies]\nlocal_alias = { path = \"../real-crate\", \
             package = \"real-crate\" }\nplain_path = { path = \"../other\" }\nserde = \
             \"1\"\n[workspace.dependencies]\nws_local = { path = \"crates/ws\" }\n",
        )
        .unwrap();
        let roots = local_crate_roots(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            roots.contains("local_alias"),
            "renamed path-dep KEY is a local root; got {roots:?}"
        );
        assert!(
            roots.contains("plain_path"),
            "path dep without `package` still local; got {roots:?}"
        );
        assert!(
            roots.contains("ws_local"),
            "[workspace.dependencies] path dep is local; got {roots:?}"
        );
        assert!(
            !roots.contains("serde"),
            "a string (external) dep is not a local root; got {roots:?}"
        );
    }

    /// #97 item 3: a BIN-ONLY member (no `[lib]` table, no `src/lib.rs`) is not importable, so it
    /// must NOT contribute its package name as a root — otherwise a same-named external dependency
    /// (`use clap::…` where a local bin is also named `clap`) is wrongly treated as local and
    /// suppression is skipped. A LIBRARY member still contributes its root.
    #[test]
    fn local_crate_roots_skips_bin_only_packages() {
        let dir = std::env::temp_dir().join(format!("rr-bin-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A bin-only crate whose name collides with the external `clap` dependency.
        std::fs::create_dir_all(dir.join("tool/src")).unwrap();
        std::fs::write(dir.join("tool/Cargo.toml"), "[package]\nname=\"clap\"\n").unwrap();
        std::fs::write(dir.join("tool/src/main.rs"), "fn main() {}").unwrap();
        // A real library member (autodiscovered src/lib.rs) that DOES contribute its root.
        std::fs::create_dir_all(dir.join("lib-crate/src")).unwrap();
        std::fs::write(dir.join("lib-crate/Cargo.toml"), "[package]\nname=\"lib-crate\"\n")
            .unwrap();
        std::fs::write(dir.join("lib-crate/src/lib.rs"), "").unwrap();
        // A member declaring an explicit [lib] table (no src/lib.rs needed).
        std::fs::create_dir_all(dir.join("explicit/src")).unwrap();
        std::fs::write(
            dir.join("explicit/Cargo.toml"),
            "[package]\nname=\"explicit\"\n[lib]\npath=\"src/thing.rs\"\n",
        )
        .unwrap();
        let roots = local_crate_roots(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !roots.contains("clap"),
            "a bin-only package name is not a local root; got {roots:?}"
        );
        assert!(
            roots.contains("lib_crate"),
            "an autodiscovered lib contributes its root; got {roots:?}"
        );
        assert!(
            roots.contains("explicit"),
            "an explicit [lib] table contributes its root; got {roots:?}"
        );
    }

    #[test]
    fn empty_local_set_fails_open() {
        let mut scope = ImportScope::new(HashSet::new());
        scope.add_use(1, "use url::Url;");
        assert!(!scope.is_external_import(1, "Url"), "no manifests → suppress nothing");
    }

    /// #97 item 1: a braced `use` longer than the 240-char `edge_evidence` cap must still mark its
    /// LATE leaf external. The Imports edges produced by extraction now carry the UNTRUNCATED use
    /// text, so the per-file scope built from that evidence sees every leaf — including one pushed
    /// past byte 240 by many earlier bindings.
    #[test]
    fn long_braced_use_marks_a_late_leaf_external() {
        // Pad with enough early leaves that `LateLeaf` lands well past char 240; each is a real
        // brace binding, so the source `use` is valid.
        let early = (0..60).map(|i| format!("Early{i}")).collect::<Vec<_>>().join(", ");
        let src = format!("use external_dep::{{{early}, LateLeaf}};");
        assert!(src.len() > 240, "fixture must exceed the truncation cap; len {}", src.len());

        // Extract real Imports edges (the path that used to truncate) and rebuild the scope from
        // their evidence, exactly as the resolve drivers do.
        let edges = crate::index::edges::syntactic_edges(
            std::path::Path::new("a.rs"),
            crate::language::Language::Rust,
            &src,
            &[],
        )
        .unwrap();
        let mut scope = ImportScope::new(HashSet::from(["mycrate".to_string()]));
        for edge in &edges {
            if edge.edge_kind == crate::index::edges::EdgeKind::Imports
                && let Some(evidence) = edge.evidence.as_deref()
            {
                scope.add_use(1, evidence);
            }
        }
        assert!(
            scope.is_external_import(1, "LateLeaf"),
            "a leaf past the 240-char cap must still be marked external"
        );
        assert!(scope.is_external_import(1, "Early0"), "early leaves stay external too");
    }
}
