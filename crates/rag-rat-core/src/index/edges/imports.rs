//! Per-package + module-aware import scope (#61). A reference to a name imported from an EXTERNAL
//! dependency crate must not bind to a local same-named symbol — the `use` says where the name
//! comes from. Two refinements over the original per-file global-set model (the 5 Codex edge cases,
//! see `docs/plans/2026-06-14-import-scope-rework.md`):
//!
//! - **Per-package locality** — a `path`-dependency alias (`local = { path = "…" }`) is
//!   manifest-LOCAL: it must make `local` a local root only for the package that declares it, not
//!   for every file in the corpus. The local-crate set is therefore per-package
//!   (`package_roots[file_package[file]]`), with the workspace crate names as the global fallback.
//! - **Module-aware scope** — a Rust `use` is scoped to its enclosing module body (or block, when
//!   block-local), and child `mod`s do NOT inherit a parent module's `use`s. A binding suppresses a
//!   reference only when the reference is lexically inside the `use`'s scope AND in the SAME module
//!   (`mod_id` match), with the INNERMOST (smallest-span) covering binding winning a shadow.
//!
//! Rust/Cargo-specific by construction: a corpus with no manifests yields empty sets and the scope
//! suppresses nothing (fail open) — a missed suppression is never a dropped local bind.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::Connection;

use super::{ImportScopeRange, MOD_FILE_ROOT};

/// Load the persisted per-repo local-crate-root set — newline-joined under `local_crate_roots` in
/// `repo_meta` (V039 relocated it there), written at rebuild by [`local_crate_roots`]. Empty when
/// absent (a non-Cargo corpus, a pre-V021 index, or a raw conn without the registry), which makes
/// [`ImportScope`] suppress nothing.
pub(crate) fn load_local_roots(conn: &Connection) -> HashSet<String> {
    crate::index::schema::single_repo_id(conn)
        .and_then(|repo_id| crate::index::repo_meta(conn, &repo_id, "local_crate_roots"))
        .ok()
        .flatten()
        .map(|value| value.lines().filter(|line| !line.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// One Cargo package discovered in the corpus: the directory of its manifest and the set of crate
/// roots that resolve LOCAL from inside it — the workspace crate names (global union) PLUS this
/// manifest's in-corpus path-dependency alias keys (#61 per-package locality).
#[derive(Debug, Clone)]
pub(crate) struct PackageRoots {
    pub(crate) manifest_dir: String,
    pub(crate) local_roots: HashSet<String>,
}

/// Scan the corpus's Cargo manifests once and return both the GLOBAL local-crate-root union (every
/// importable workspace crate name + every in-corpus path-dep alias key, the fallback set) and the
/// PER-PACKAGE roots (one [`PackageRoots`] per manifest, each = the global workspace crate names
/// plus that manifest's own in-corpus path-dep alias keys). Built in one walk so
/// rebuild/incremental pay the manifest I/O once.
pub(crate) fn scan_packages(root: &Path) -> (HashSet<String>, Vec<PackageRoots>) {
    // First pass: every importable LIBRARY crate root in the corpus — the workspace crate names
    // shared by every package as local. `parents(false)`: honor the indexed root's OWN .gitignore
    // (skip its `target/` vendored manifests) but NOT ancestor gitignores above it — otherwise a
    // root nested under some ancestor's ignored path (e.g. a bench corpus under `target/`) is
    // treated as wholly ignored and the walk yields nothing.
    let mut workspace_roots = HashSet::new();
    let mut manifests: Vec<(std::path::PathBuf, toml::Value)> = Vec::new();
    for entry in ignore::WalkBuilder::new(root).parents(false).build().flatten() {
        if entry.path().file_name().and_then(|name| name.to_str()) != Some("Cargo.toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        // Parse with the serde entry point (`toml::from_str`), NOT `text.parse::<toml::Value>()`:
        // `toml::Value`'s `FromStr` compiles but does not parse a full Cargo.toml document — it
        // returns Err on a valid manifest, silently emptying the local set.
        let Ok(manifest) = toml::from_str::<toml::Value>(&text) else { continue };
        let manifest_dir = entry.path().parent().map(Path::to_path_buf);
        if let Some(name) = lib_crate_root_name(&manifest, manifest_dir.as_deref()) {
            workspace_roots.insert(name);
        }
        if let Some(dir) = manifest_dir {
            manifests.push((dir, manifest));
        }
    }
    // The workspace root: the manifest declaring `[workspace]`. Its `[workspace.dependencies]`
    // table holds the `path` for any member dep written `{ workspace = true }`, and that path
    // is relative to the WORKSPACE ROOT dir (not the inheriting member's). Captured once so the
    // per-package pass can resolve inherited path-dep aliases (#5).
    let workspace_deps = manifests
        .iter()
        .find(|(_, manifest)| manifest.get("workspace").is_some())
        .map(|(dir, manifest)| WorkspaceDeps {
            root_dir: dir.clone(),
            table: manifest
                .get("workspace")
                .and_then(|workspace| workspace.get("dependencies"))
                .and_then(toml::Value::as_table)
                .cloned(),
        });
    // Second pass: per-package roots = the workspace union plus this manifest's in-corpus path-dep
    // alias keys (only that package treats those aliases as local — #1).
    let mut packages = Vec::with_capacity(manifests.len());
    let mut global_roots = workspace_roots.clone();
    for (manifest_dir, manifest) in &manifests {
        let mut local_roots = workspace_roots.clone();
        collect_path_dependency_aliases(
            manifest,
            manifest_dir,
            root,
            workspace_deps.as_ref(),
            &mut local_roots,
        );
        // The global fallback union also includes every alias key, so a file with no package row
        // still fails open the same way the pre-#61 single-set did.
        global_roots.extend(local_roots.iter().cloned());
        // Store `manifest_dir` RELATIVE to the indexed root (`/`-normalized) so it prefix-matches a
        // file's stored relative path when assigning `files.package_id`. The root manifest yields
        // an empty string, which is a prefix of every path (the root crate owns top-level files).
        let relative = manifest_dir
            .strip_prefix(root)
            .unwrap_or(manifest_dir)
            .to_string_lossy()
            .replace('\\', "/");
        packages.push(PackageRoots { manifest_dir: relative, local_roots });
    }
    (global_roots, packages)
}

/// The crate root identifier used in `use` paths when this manifest defines an importable LIBRARY:
/// `[lib].name` if the table sets one, else the package name (`-`→`_`, Cargo's crate-name rule:
/// `cargo-credential` → `cargo_credential`). Returns `None` for a BIN-ONLY member (no `[lib]` table
/// and no autodiscovered `src/lib.rs`): such a package is not importable, so contributing its name
/// as a root would wrongly mark a same-named EXTERNAL dependency as local and skip suppression
/// (#97 item 3). The `src/lib.rs` probe mirrors Cargo's lib autodiscovery, resolved relative to the
/// manifest directory; `[package] autolib = false` disables that autodiscovery (#97 item 4). An
/// EXPLICIT `[lib]` table still contributes regardless of `autolib` — that flag only governs
/// autodiscovery.
fn lib_crate_root_name(manifest: &toml::Value, manifest_dir: Option<&Path>) -> Option<String> {
    let lib = manifest.get("lib");
    if let Some(lib_name) = lib.and_then(|lib| lib.get("name")).and_then(|name| name.as_str()) {
        return Some(lib_name.replace('-', "_"));
    }
    let autolib_enabled = manifest
        .get("package")
        .and_then(|package| package.get("autolib"))
        .and_then(toml::Value::as_bool)
        != Some(false);
    let has_lib_target = lib.is_some()
        || (autolib_enabled
            && manifest_dir.is_some_and(|dir| dir.join("src").join("lib.rs").is_file()));
    if !has_lib_target {
        return None;
    }
    let package = manifest.get("package")?.get("name")?.as_str()?;
    Some(package.replace('-', "_"))
}

/// The workspace root's `[workspace.dependencies]` table and the dir it lives in. A member dep
/// written `local = { workspace = true }` inherits its real `path` from this table, and that path
/// is relative to `root_dir` (the workspace root), not the inheriting member's dir (#5).
struct WorkspaceDeps {
    root_dir: std::path::PathBuf,
    table: Option<toml::value::Table>,
}

/// Add every path-dependency KEY whose target resolves INSIDE `root` to `roots` — these are local
/// path dependencies whose import name is the KEY (possibly renamed via the `package` key), so the
/// bare-reference scope must treat the key as a local crate root (#97 item 2). A string dependency
/// (`serde = "1"`) or a non-`path` table dependency is external and contributes nothing.
///
/// Scans the normal/dev/build dependency tables, every `[target.*.…]` table, and
/// `[workspace.dependencies]`: aliases declared for tests/examples/build scripts/cfg-specific
/// modules feed indexed Rust too, so a `use local_alias::…` from any of them must resolve local
/// (#97 item 4). The key is `-`→`_` normalized (a dependency KEY may contain `-` yet imports as
/// `use foo_bar::…`). A `path` pointing OUTSIDE the indexed `root` is NOT added (#97 item 5): that
/// crate has no symbols in this index, so localizing its alias would let a same-named in-corpus
/// symbol wrongly bind.
///
/// A member dep `local = { workspace = true }` carries no `path` of its own — its path lives in the
/// workspace root's `[workspace.dependencies]` and is relative to the WORKSPACE ROOT dir. Such a
/// dep is resolved against `workspace_deps` so the inherited path-dep alias is still treated as
/// local in the member (#5); without it, `use local::Thing` in the member was misclassified
/// external.
fn collect_path_dependency_aliases(
    manifest: &toml::Value,
    manifest_dir: &Path,
    root: &Path,
    workspace_deps: Option<&WorkspaceDeps>,
    roots: &mut HashSet<String>,
) {
    for table in dependency_tables(manifest) {
        for (key, spec) in table {
            // A direct `path` on the dep: resolve relative to THIS manifest's dir.
            if let Some(path) = spec.get("path").and_then(toml::Value::as_str) {
                if path_dependency_is_in_corpus(manifest_dir, path, root) {
                    roots.insert(key.replace('-', "_"));
                }
                continue;
            }
            // `{ workspace = true }`: inherit the `path` from `[workspace.dependencies]`, resolved
            // relative to the WORKSPACE ROOT dir (#5).
            let inherits_workspace =
                spec.get("workspace").and_then(toml::Value::as_bool) == Some(true);
            if inherits_workspace
                && let Some(ws) = workspace_deps
                && let Some(path) = ws
                    .table
                    .as_ref()
                    .and_then(|table| table.get(key))
                    .and_then(|inherited| inherited.get("path"))
                    .and_then(toml::Value::as_str)
                && path_dependency_is_in_corpus(&ws.root_dir, path, root)
            {
                roots.insert(key.replace('-', "_"));
            }
        }
    }
}

/// Every dependency table in a manifest that can carry a path alias bound into indexed Rust: the
/// top-level `[dependencies]` / `[dev-dependencies]` / `[build-dependencies]`,
/// `[workspace.dependencies]`, and each per-platform `[target.<cfg>.{dependencies,…}]` table.
fn dependency_tables(manifest: &toml::Value) -> Vec<&toml::value::Table> {
    const KINDS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut tables = Vec::new();
    for kind in KINDS {
        tables.extend(manifest.get(kind).and_then(toml::Value::as_table));
    }
    tables.extend(
        manifest
            .get("workspace")
            .and_then(|workspace| workspace.get("dependencies"))
            .and_then(toml::Value::as_table),
    );
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for cfg in targets.values() {
            for kind in KINDS {
                tables.extend(cfg.get(kind).and_then(toml::Value::as_table));
            }
        }
    }
    tables
}

/// Whether a path dependency's target directory resolves under the indexed `root` (#97 item 5).
/// `manifest_dir` is the directory of the manifest declaring the dependency; `path` is the relative
/// `path = "…"` value. Canonicalizes both so `../` traversal and symlinks compare honestly; an
/// unresolvable target (not present on disk) can't hold indexed symbols, so it's treated as
/// outside.
fn path_dependency_is_in_corpus(manifest_dir: &Path, path: &str, root: &Path) -> bool {
    let Ok(target) = manifest_dir.join(path).canonicalize() else { return false };
    match root.canonicalize() {
        Ok(canonical_root) => target.starts_with(&canonical_root),
        Err(_) => target.starts_with(root),
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

/// One `use`-introduced binding of a leaf name: the crate root it came from and the module-aware
/// scope it applies to (#61). A reference is suppressed by this binding only when it falls inside
/// `[scope_start, scope_end)` AND its enclosing-module id equals `mod_id` — so `mod a { use
/// url::Url }` does not suppress a local `Url` in a sibling/child `mod b`. On multiple covering
/// bindings the INNERMOST (smallest span) wins (inner-local `use` shadows an outer-external one).
#[derive(Clone, Debug)]
struct ImportBinding {
    root: String,
    scope_start: usize,
    scope_end: usize,
    mod_id: i64,
}

impl ImportBinding {
    fn span(&self) -> usize {
        self.scope_end.saturating_sub(self.scope_start)
    }

    /// Whether a reference at `ref_byte` whose enclosing module is `ref_mod_id` is covered by this
    /// binding: lexically inside the scope (half-open) AND in the same module body.
    fn covers(&self, ref_byte: usize, ref_mod_id: i64) -> bool {
        ref_byte >= self.scope_start && ref_byte < self.scope_end && ref_mod_id == self.mod_id
    }
}

/// Per-file, module-aware map of imported leaf name → its `use` bindings, plus the per-package and
/// global local-crate sets and the per-file module-range interval set, so resolution can tell an
/// external-dependency import from a local one WITHOUT the tree.
#[derive(Default)]
pub(crate) struct ImportScope {
    by_file: HashMap<i64, HashMap<String, Vec<ImportBinding>>>,
    /// Per file, the inline-module body ranges as `(start, end, mod_id)`, sorted + de-duplicated
    /// by [`Self::finalize`] — built ONCE per file on ingest so the ref→mod-id lookup stays
    /// off the per-edge hot path.
    module_ranges: HashMap<i64, Vec<(usize, usize, i64)>>,
    file_package: HashMap<i64, i64>,
    package_roots: HashMap<i64, HashSet<String>>,
    global_roots: HashSet<String>,
    /// Whether the indexed corpus is a Cargo project at all (at least one `Cargo.toml` found).
    /// This is DISTINCT from an empty root set: a Cargo project of only BIN targets has no
    /// importable library crate names and no path-dep aliases, so its root set is empty — yet
    /// `use url::Url;` in it still names the EXTERNAL `url` crate and must suppress a local
    /// `Url`. Only a genuinely non-Cargo corpus (no manifest at all) fails open. Set true by
    /// [`Self::mark_has_manifests`] when any package row is loaded.
    has_manifests: bool,
    /// Per-file Python import aliases: `alias name → its bindings` (#174). A `from m import T as
    /// A` records `A → T` so a later reference to `A` resolves to the IMPORTED symbol `T`, not
    /// an unrelated local `A`. Distinct from `by_file` (Rust external-import suppression):
    /// this REBINDS an alias use to its in-corpus target name, rather than just flagging it
    /// external.
    python_aliases: HashMap<i64, HashMap<String, Vec<PythonAlias>>>,
}

/// One Python import alias binding: the imported target name the alias stands for, scoped to a byte
/// range (whole-file today — Python imports are module-global). Mirrors [`ImportBinding`] but
/// carries the TARGET name (for rebinding) rather than the crate root (for external detection).
struct PythonAlias {
    target: String,
    scope_start: usize,
    scope_end: usize,
}

impl PythonAlias {
    fn covers(&self, ref_byte: usize) -> bool {
        ref_byte >= self.scope_start && ref_byte < self.scope_end
    }
}

impl ImportScope {
    pub(crate) fn new(global_roots: HashSet<String>) -> Self {
        Self { global_roots, ..Self::default() }
    }

    /// Record that the corpus has at least one Cargo manifest — so an empty root set is read as
    /// "Cargo project with no library crates" (still suppress external imports), not "non-Cargo
    /// corpus" (fail open). See [`Self::has_manifests`].
    pub(crate) fn mark_has_manifests(&mut self) {
        self.has_manifests = true;
    }

    /// Map a file to its owning package (so its bindings consult that package's local-crate set).
    pub(crate) fn set_file_package(&mut self, file_id: i64, package_id: i64) {
        self.file_package.insert(file_id, package_id);
    }

    /// Record a package's local-crate-root set (workspace crates + that manifest's in-corpus path-
    /// dep alias keys).
    pub(crate) fn set_package_roots(&mut self, package_id: i64, roots: HashSet<String>) {
        self.package_roots.insert(package_id, roots);
    }

    /// Record a Python `from <module> import <target> as <alias>` binding (#174): `alias → target`,
    /// scoped to the import's byte range. The carrier is the aliased import's Imports edge — its
    /// `to_name` is `target` and its `evidence` is `alias` (set by extraction's
    /// `python_import_target`).
    pub(crate) fn add_python_alias(
        &mut self,
        file_id: i64,
        alias: String,
        target: String,
        scope: Option<ImportScopeRange>,
    ) {
        // A binding with no scope can't be range-tested; skip rather than bind file-wide blindly.
        let Some(scope) = scope else { return };
        self.python_aliases.entry(file_id).or_default().entry(alias).or_default().push(
            PythonAlias { target, scope_start: scope.scope_start, scope_end: scope.scope_end },
        );
    }

    /// The imported target name a Python alias `name` stands for at `ref_byte`, if any (#174). The
    /// caller resolves the reference under this target name instead of the alias — so `Account()`
    /// after `from models import User as Account` binds to `User`. `None` for non-Python files (the
    /// map is empty) or a name that isn't an in-scope alias.
    pub(crate) fn python_alias_target(
        &self,
        file_id: i64,
        name: &str,
        ref_byte: usize,
    ) -> Option<&str> {
        // The bindings covering `ref_byte` (#174 review). A sequential re-import
        // (`from m1 import User as Account; from m2 import Customer as Account`) leaves exactly ONE
        // covering binding at any byte — extraction's `python_next_module_binding` shrinks each
        // earlier binding's `scope_end` to the next UNCONDITIONAL rebinding, so the ranges abut.
        // The only way two cover the same byte is mutually-exclusive CONDITIONAL branches
        // (`try: import … as A except: import … as A`), neither of which shrinks the other; when
        // those name DIFFERENT targets the alias is genuinely AMBIGUOUS, so resolve nothing rather
        // than pick one by byte order. All-same-target overlaps (try/except importing the same
        // symbol from different modules) still resolve.
        let mut covering = self
            .python_aliases
            .get(&file_id)?
            .get(name)?
            .iter()
            .filter(|alias| alias.covers(ref_byte));
        let first = covering.next()?;
        if covering.any(|alias| alias.target != first.target) {
            return None;
        }
        Some(first.target.as_str())
    }

    /// Record one Imports edge for `file_id`. An inline-`mod` edge (scope present, but `use_text`
    /// not a `use`) contributes only a module-range interval; a `use` edge records a binding per
    /// leaf name with the edge's module-aware scope. Idempotent — the same evidence arrives once
    /// per identifier in the imports edge stream, and the leaf map de-dups exact repeats;
    /// module ranges are de-duplicated when [`Self::finalize`] runs.
    pub(crate) fn add_use(
        &mut self,
        file_id: i64,
        use_text: &str,
        scope: Option<ImportScopeRange>,
    ) {
        if let Some(scope) = scope {
            // An inline `mod foo { … }` edge carries its OWN body as the scope with
            // `mod_id == scope_start` — these populate the module interval set (the ref→mod-id
            // lookup), including modules with no `use`. A `use` edge's scope `mod_id` is its
            // ENCLOSING module's start (≠ its own scope_start unless top-level), so only `mod`
            // edges contribute a self-range; recording every edge's scope here is still correct,
            // since a `use`'s `[scope_start, scope_end)` for a module-level use IS that module's
            // body — `finalize` de-dups the overlap.
            self.module_ranges.entry(file_id).or_default().push((
                scope.scope_start,
                scope.scope_end,
                scope.mod_id,
            ));
        }
        let Some((root, leaves)) = parse_use(use_text) else { return };
        let (scope_start, scope_end, mod_id) = match scope {
            Some(scope) => (scope.scope_start, scope.scope_end, scope.mod_id),
            // A pre-#61 import edge (or a test) with no scope columns: fall open to whole-file,
            // file-root scope — reproduces the original per-file behavior.
            None => (0, usize::MAX, MOD_FILE_ROOT),
        };
        let file = self.by_file.entry(file_id).or_default();
        for leaf in leaves {
            let binding = ImportBinding { root: root.clone(), scope_start, scope_end, mod_id };
            let bindings = file.entry(leaf).or_default();
            // Dedup exact repeats (the same leaf re-emitted under the truncated-evidence edges of
            // one `use`); a different scope/root is a distinct binding and is kept.
            if !bindings.iter().any(|b| {
                b.root == binding.root
                    && b.scope_start == binding.scope_start
                    && b.scope_end == binding.scope_end
                    && b.mod_id == binding.mod_id
            }) {
                bindings.push(binding);
            }
        }
    }

    /// Finalize the per-file module-range interval sets: sort each by `(start asc, end desc)` and
    /// drop exact duplicates. Called ONCE after all edges are ingested, so the ref→mod-id lookup
    /// stays cheap and off the per-edge hot path. Idempotent.
    pub(crate) fn finalize(&mut self) {
        for ranges in self.module_ranges.values_mut() {
            ranges.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
            ranges.dedup();
        }
    }

    /// The id of the INNERMOST inline module whose body contains `ref_byte`, or [`MOD_FILE_ROOT`]
    /// when the reference is at file root. "Innermost" = the smallest containing body. O(n) over a
    /// file's module ranges (a handful), already sorted by [`Self::finalize`].
    fn ref_mod_id(&self, file_id: i64, ref_byte: usize) -> i64 {
        let Some(ranges) = self.module_ranges.get(&file_id) else {
            return MOD_FILE_ROOT;
        };
        let mut best: Option<(usize, i64)> = None; // (span, mod_id)
        for &(start, end, mod_id) in ranges {
            // A `use` edge contributes ranges with `mod_id != start` (its enclosing module); only a
            // real inline-`mod` self-range (mod_id == start) defines a module the ref can belong
            // to.
            if mod_id != start as i64 {
                continue;
            }
            if ref_byte >= start && ref_byte < end {
                let span = end - start;
                if best.is_none_or(|(best_span, _)| span <= best_span) {
                    best = Some((span, mod_id));
                }
            }
        }
        best.map(|(_, mod_id)| mod_id).unwrap_or(MOD_FILE_ROOT)
    }

    /// The local-crate-root set that applies to `file_id`: its package's set if known, else the
    /// global union (a file with no package row falls open exactly like the pre-#61 single-set).
    fn roots_for_file(&self, file_id: i64) -> &HashSet<String> {
        self.file_package
            .get(&file_id)
            .and_then(|package_id| self.package_roots.get(package_id))
            .unwrap_or(&self.global_roots)
    }

    /// The crate root that binds `name` at `ref_byte` in `file_id`, if any — the INNERMOST covering
    /// `use` binding's root (smallest span wins, so an inner-local `use` shadows an outer one). The
    /// module-aware covering test stops a parent-`mod` `use` from reaching a child-`mod` reference.
    fn covering_root(&self, file_id: i64, name: &str, ref_byte: usize) -> Option<&str> {
        let bindings = self.by_file.get(&file_id)?.get(name)?;
        let ref_mod_id = self.ref_mod_id(file_id, ref_byte);
        bindings
            .iter()
            .filter(|binding| binding.covers(ref_byte, ref_mod_id))
            .min_by_key(|binding| binding.span())
            .map(|binding| binding.root.as_str())
    }

    /// Whether `name` at `ref_byte` in `file_id` was imported from an EXTERNAL dependency crate —
    /// not a local workspace/path-dep crate, not `crate`/`self`/`super`. When true, the name
    /// denotes that dependency's item and must NOT bind to a local same-named symbol. Fails
    /// OPEN: with no local-crate set (non-Cargo corpus / manifest scan found nothing), nothing
    /// is suppressed.
    pub(crate) fn is_external_import(&self, file_id: i64, name: &str, ref_byte: usize) -> bool {
        let local_roots = self.roots_for_file(file_id);
        // Fail open ONLY for a non-Cargo corpus (no manifest scanned). A Cargo project with no
        // library crates (bin-only) has an empty root set but still suppresses external imports —
        // `use url::Url;` names the external `url` crate there too (#97-followup item 4).
        if local_roots.is_empty() && !self.has_manifests {
            return false;
        }
        let Some(root) = self.covering_root(file_id, name, ref_byte) else {
            return false;
        };
        !local_roots.contains(root) && !matches!(root, "crate" | "self" | "super")
    }

    /// Whether a path-qualified reference's RECEIVER/root names an external import — `Url::parse`
    /// (target_qualified_name `Url::parse`) where `Url` was `use`d from the external `url` crate.
    /// The leaf `parse` itself isn't imported, so [`is_external_import`] on the callee name misses
    /// it; this checks the receiver root.
    ///
    /// Gated on a TYPE-LIKE (uppercase) head. `target_qualified_name` rewrites `.`→`::`, so a
    /// value-receiver method call `config.build()` arrives as `config::build` and is
    /// INDISTINGUISHABLE from a path call by punctuation alone; suppressing it would drop a valid
    /// local method call. The uppercase gate keeps the PascalCase-type case (`Url::parse`) and lets
    /// snake_case value/module heads fall through (fail open — a missed suppression, never a
    /// dropped local bind).
    pub(crate) fn is_external_qualified_root(
        &self,
        file_id: i64,
        target_qualified_name: Option<&str>,
        ref_byte: usize,
    ) -> bool {
        target_qualified_name.and_then(|qualified| qualified.split_once("::")).is_some_and(
            |(root, _)| {
                root.starts_with(char::is_uppercase)
                    && self.is_external_import(file_id, root, ref_byte)
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

    /// An inline-module body range as an [`ImportScopeRange`] (`mod_id == scope_start`).
    fn mod_scope(start: usize, end: usize) -> ImportScopeRange {
        ImportScopeRange { scope_start: start, scope_end: end, mod_id: start as i64 }
    }

    /// A `use` scope inside module `mod_id` (a body start), covering `[start, end)`.
    fn use_in_mod(start: usize, end: usize, mod_id: i64) -> ImportScopeRange {
        ImportScopeRange { scope_start: start, scope_end: end, mod_id }
    }

    fn scope_at_file_root(start: usize, end: usize) -> ImportScopeRange {
        ImportScopeRange { scope_start: start, scope_end: end, mod_id: MOD_FILE_ROOT }
    }

    #[test]
    fn parse_use_extracts_root_and_only_bound_leaves() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(parsed("use url::Url;"), Some(("url".into(), s(&["Url"]))));
        assert_eq!(
            parsed("use cargo::core::Workspace;"),
            Some(("cargo".into(), s(&["Workspace"])))
        );
        assert_eq!(
            parsed("use std::path::{Path, PathBuf};"),
            Some(("std".into(), s(&["Path", "PathBuf"])))
        );
        assert_eq!(parsed("use std::path;"), Some(("std".into(), s(&["path"]))));
        assert_eq!(parsed("pub(crate) use url::Url;"), Some(("url".into(), s(&["Url"]))));
        assert_eq!(parsed("pub(super) use a::B;"), Some(("a".into(), s(&["B"]))));
        assert_eq!(parsed("pub(in crate::x) use a::B;"), Some(("a".into(), s(&["B"]))));
        assert_eq!(parsed("pub use crate::a::B;"), Some(("crate".into(), s(&["B"]))));
        assert_eq!(parsed("use foo::Bar as Baz;"), Some(("foo".into(), s(&["Baz"]))));
        assert_eq!(parsed("use my_crate as local;"), Some(("my_crate".into(), s(&["local"]))));
        assert_eq!(parsed("use foo::{Bar as Baz, Qux};"), Some(("foo".into(), s(&["Baz", "Qux"]))));
        assert_eq!(parsed("use a::b::{self, C};"), Some(("a".into(), s(&["C", "b"]))));
        assert_eq!(parsed("use a::{b::{C, D}, E};"), Some(("a".into(), s(&["C", "D", "E"]))));
        assert_eq!(parse_use("use foo::*;"), Some(("foo".to_string(), Vec::new())));
        assert_eq!(parsed("use ::external::X;"), Some(("external".into(), s(&["X"]))));
        assert_eq!(parse_use("mod foo;"), None);
    }

    #[test]
    fn external_import_distinguishes_local_from_dependency() {
        let mut scope = ImportScope::new(HashSet::from(["cargo".to_string()]));
        let s = Some(scope_at_file_root(0, usize::MAX));
        scope.add_use(1, "use url::Url;", s); // external dep
        scope.add_use(1, "use cargo::core::Workspace;", s); // local workspace crate
        scope.add_use(1, "use std::path::{Path, PathBuf};", s); // std = external
        scope.add_use(1, "use crate::a::Helper;", s); // local (crate-relative)
        scope.finalize();
        assert!(scope.is_external_import(1, "Url", 100), "url is an external dep");
        assert!(scope.is_external_import(1, "Path", 100), "std is external");
        assert!(!scope.is_external_import(1, "Workspace", 100), "cargo is a local workspace crate");
        assert!(!scope.is_external_import(1, "Helper", 100), "crate-rooted is local");
        assert!(
            !scope.is_external_import(1, "path", 100),
            "the `std::path` PREFIX is not a binding — a local `path` must stay resolvable"
        );
        assert!(
            !scope.is_external_import(1, "Unimported", 100),
            "an unimported name is never suppressed"
        );
        assert!(!scope.is_external_import(2, "Url", 100), "scoped per file");
    }

    #[test]
    fn empty_local_set_fails_open() {
        let mut scope = ImportScope::new(HashSet::new());
        scope.add_use(1, "use url::Url;", Some(scope_at_file_root(0, usize::MAX)));
        scope.finalize();
        assert!(!scope.is_external_import(1, "Url", 100), "no manifests → suppress nothing");
    }

    /// #4: a bin-only Cargo crate has an EMPTY local-root set (no importable lib name, no path-dep
    /// aliases) yet IS a Cargo project — `use url::Url;` names the external `url` crate and must
    /// still suppress a local `Url`. Only a genuinely non-Cargo corpus fails open. The
    /// discriminator is `mark_has_manifests`, not the emptiness of the root set.
    #[test]
    fn bin_only_crate_still_suppresses_external_imports() {
        let mut scope = ImportScope::new(HashSet::new());
        scope.mark_has_manifests(); // a Cargo manifest exists, just no library crate roots
        scope.add_use(1, "use url::Url;", Some(scope_at_file_root(0, usize::MAX)));
        scope.finalize();
        assert!(
            scope.is_external_import(1, "Url", 100),
            "a bin-only Cargo crate still suppresses an external import despite an empty root set"
        );
    }

    /// #1 (per-package locality): a `path`-dep alias `local` is local for crate A (which declares
    /// it) but EXTERNAL for crate B (with its own same-named import). One global set can't express
    /// that; the per-package set must.
    #[test]
    fn per_package_alias_does_not_leak_across_packages() {
        let mut scope = ImportScope::new(HashSet::from(["myws".to_string(), "local".to_string()]));
        scope.set_package_roots(10, HashSet::from(["myws".to_string(), "local".to_string()]));
        scope.set_package_roots(20, HashSet::from(["myws".to_string()]));
        scope.set_file_package(1, 10); // file 1 → package A
        scope.set_file_package(2, 20); // file 2 → package B
        scope.add_use(1, "use local::Thing;", Some(scope_at_file_root(0, usize::MAX)));
        scope.add_use(2, "use local::Thing;", Some(scope_at_file_root(0, usize::MAX)));
        scope.finalize();
        assert!(
            !scope.is_external_import(1, "Thing", 100),
            "in package A, `local` is its own path-dep alias — a LOCAL crate"
        );
        assert!(
            scope.is_external_import(2, "Thing", 100),
            "in package B, `local` is NOT a declared alias — it names an EXTERNAL crate"
        );
    }

    /// #4 (parent-`mod` `use` does not suppress a child-`mod` local): the use in `mod a` must not
    /// reach a `Url` reference inside the child `mod b` — child modules don't inherit a parent's
    /// `use`s. A reference in `mod a` itself IS covered.
    #[test]
    fn parent_module_use_does_not_suppress_child_mod_local() {
        let mut scope = ImportScope::new(HashSet::from(["myws".to_string()]));
        scope.add_use(1, "mod a", Some(mod_scope(0, 200))); // mod a body [0,200)
        scope.add_use(1, "mod b", Some(mod_scope(80, 160))); // nested mod b body [80,160)
        scope.add_use(1, "use url::Url;", Some(use_in_mod(0, 200, 0))); // use lives in mod a
        scope.finalize();
        assert!(
            !scope.is_external_import(1, "Url", 100),
            "a's `use url::Url` must not reach a reference inside child mod b"
        );
        assert!(
            scope.is_external_import(1, "Url", 40),
            "a reference in mod a itself IS covered by a's `use url::Url`"
        );
    }

    /// #5 (innermost shadowing): an inner-local `use crate::Url` and an outer-external `use url::Url`
    /// both lexically cover a reference; the INNERMOST (smallest span, same module) wins → resolve
    /// LOCAL.
    #[test]
    fn innermost_use_wins_over_outer() {
        let mut scope = ImportScope::new(HashSet::from(["myws".to_string()]));
        scope.add_use(1, "mod a", Some(mod_scope(0, 300)));
        scope.add_use(1, "mod b", Some(mod_scope(100, 200)));
        scope.add_use(1, "use url::Url;", Some(use_in_mod(0, 300, 0))); // outer external, mod a
        scope.add_use(1, "use crate::Url;", Some(use_in_mod(100, 200, 100))); // inner local, mod b
        scope.finalize();
        assert!(
            !scope.is_external_import(1, "Url", 150),
            "the inner-local `use crate::Url` shadows the outer-external one"
        );
    }

    /// The `use`'s scope is the MODULE body, not a `block` inside it that merely INHERITS the
    /// module's uses; a block-local `use` is confined to its block (#61).
    #[test]
    fn use_scope_is_module_not_block() {
        let mut scope = ImportScope::new(HashSet::from(["myws".to_string()]));
        scope.add_use(1, "mod a", Some(mod_scope(0, 300)));
        scope.add_use(1, "use url::Url;", Some(use_in_mod(0, 300, 0))); // module-level use
        scope.finalize();
        assert!(
            scope.is_external_import(1, "Url", 80),
            "a module-level `use` covers a reference in a nested block of the same module"
        );

        let mut block_scope = ImportScope::new(HashSet::from(["myws".to_string()]));
        block_scope.add_use(1, "mod a", Some(mod_scope(0, 300)));
        // Block-local use: scope is the block [50,120), mod_id still 0 (the enclosing module).
        block_scope.add_use(1, "use url::Url;", Some(use_in_mod(50, 120, 0)));
        block_scope.finalize();
        assert!(
            block_scope.is_external_import(1, "Url", 80),
            "a block-local use covers a reference inside the block"
        );
        assert!(
            !block_scope.is_external_import(1, "Url", 200),
            "a block-local use does NOT reach a reference outside the block (same module)"
        );
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
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join("crates/foo-bar/Cargo.toml"), "[package]\nname=\"foo-bar\"\n")
            .unwrap();
        std::fs::write(dir.join("crates/foo-bar/src/lib.rs"), "").unwrap();
        let roots = scan_packages(&dir).0;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(roots.contains("cargo"), "top package; got {roots:?}");
        assert!(roots.contains("foo_bar"), "hyphen→underscore normalized; got {roots:?}");
    }

    /// #97 item 3: a BIN-ONLY member (no `[lib]`, no `src/lib.rs`) is not importable, so it must NOT
    /// contribute its package name as a root — else a same-named external dependency is wrongly
    /// treated as local. A LIBRARY member (autodiscovered `src/lib.rs` or explicit `[lib]`) does.
    #[test]
    fn local_crate_roots_skips_bin_only_packages() {
        let dir = std::env::temp_dir().join(format!("rr-bin-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tool/src")).unwrap();
        std::fs::write(dir.join("tool/Cargo.toml"), "[package]\nname=\"clap\"\n").unwrap();
        std::fs::write(dir.join("tool/src/main.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(dir.join("lib-crate/src")).unwrap();
        std::fs::write(dir.join("lib-crate/Cargo.toml"), "[package]\nname=\"lib-crate\"\n")
            .unwrap();
        std::fs::write(dir.join("lib-crate/src/lib.rs"), "").unwrap();
        std::fs::create_dir_all(dir.join("explicit/src")).unwrap();
        std::fs::write(
            dir.join("explicit/Cargo.toml"),
            "[package]\nname=\"explicit\"\n[lib]\npath=\"src/thing.rs\"\n",
        )
        .unwrap();
        let roots = scan_packages(&dir).0;
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

    /// #97 item 5: a `path = "…"` dependency pointing OUTSIDE the indexed root has no symbols in
    /// this index, so localizing its alias would let a same-named in-corpus symbol wrongly bind.
    #[test]
    fn local_crate_roots_excludes_out_of_corpus_path_deps() {
        let base = std::env::temp_dir().join(format!("rr-corpus-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("indexed");
        std::fs::create_dir_all(base.join("sibling/src")).unwrap();
        std::fs::write(base.join("sibling/src/lib.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("inside")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"app\"\n[dependencies]\noutside_alias = { path = \"../sibling\" \
             }\ninside_alias = { path = \"inside\" }\nmissing_alias = { path = \"nope\" }\n",
        )
        .unwrap();
        let roots = scan_packages(&root).0;
        let _ = std::fs::remove_dir_all(&base);
        assert!(
            !roots.contains("outside_alias"),
            "a path dep outside the indexed root is NOT a local root; got {roots:?}"
        );
        assert!(
            roots.contains("inside_alias"),
            "a path dep inside the indexed root IS a local root; got {roots:?}"
        );
        assert!(
            !roots.contains("missing_alias"),
            "a path dep whose target doesn't exist can't hold indexed symbols; got {roots:?}"
        );
    }

    /// `scan_packages` returns per-package roots: each manifest's set is the workspace union plus
    /// only ITS OWN in-corpus path-dep alias keys (#1 — the cross-package leak guard at the
    /// source).
    #[test]
    fn scan_packages_keeps_path_dep_aliases_package_local() {
        let root = std::env::temp_dir().join(format!("rr-scan-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("shared/src")).unwrap();
        std::fs::write(root.join("shared/Cargo.toml"), "[package]\nname=\"shared\"\n").unwrap();
        std::fs::write(root.join("shared/src/lib.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("a/src")).unwrap();
        std::fs::write(
            root.join("a/Cargo.toml"),
            "[package]\nname=\"a\"\n[dependencies]\na_only = { path = \"../shared\" }\n",
        )
        .unwrap();
        std::fs::write(root.join("a/src/lib.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("b/src")).unwrap();
        std::fs::write(root.join("b/Cargo.toml"), "[package]\nname=\"b\"\n").unwrap();
        std::fs::write(root.join("b/src/lib.rs"), "").unwrap();
        let (global, packages) = scan_packages(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            global.contains("a_only"),
            "the global union includes every alias key; got {global:?}"
        );
        let pkg_a = packages.iter().find(|p| p.manifest_dir == "a").expect("package a");
        let pkg_b = packages.iter().find(|p| p.manifest_dir == "b").expect("package b");
        assert!(
            pkg_a.local_roots.contains("a_only"),
            "package a owns the alias; got {:?}",
            pkg_a.local_roots
        );
        assert!(
            !pkg_b.local_roots.contains("a_only"),
            "package b never declared the alias — must not be local for it; got {:?}",
            pkg_b.local_roots
        );
    }

    /// #5: a member dep written `local = { workspace = true }` inherits its `path` from the root
    /// `[workspace.dependencies]` table (path relative to the WORKSPACE ROOT dir). The member must
    /// treat `local` as a local crate root so `use local::Thing` resolves local — without inherited
    /// resolution it was dropped as external.
    #[test]
    fn scan_packages_resolves_inherited_workspace_path_dep() {
        let root = std::env::temp_dir().join(format!("rr-ws-inherit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // Workspace root declares the path dep in [workspace.dependencies]; path is relative to the
        // workspace root dir.
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=[\"member\"]\n[package]\nname=\"ws_root\"\n[workspace.\
             dependencies]\nlocal = { path = \"shared\" }\n",
        )
        .unwrap();
        // The shared crate the alias points at.
        std::fs::create_dir_all(root.join("shared/src")).unwrap();
        std::fs::write(root.join("shared/Cargo.toml"), "[package]\nname=\"shared\"\n").unwrap();
        std::fs::write(root.join("shared/src/lib.rs"), "").unwrap();
        // The member inherits the dep with `{ workspace = true }` — no path of its own.
        std::fs::create_dir_all(root.join("member/src")).unwrap();
        std::fs::write(
            root.join("member/Cargo.toml"),
            "[package]\nname=\"member\"\n[dependencies]\nlocal = { workspace = true }\n",
        )
        .unwrap();
        std::fs::write(root.join("member/src/lib.rs"), "").unwrap();
        let (global, packages) = scan_packages(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            global.contains("local"),
            "the inherited workspace path-dep alias is in the global union; got {global:?}"
        );
        let member = packages.iter().find(|p| p.manifest_dir == "member").expect("member package");
        assert!(
            member.local_roots.contains("local"),
            "the member inherits the workspace path-dep alias as a LOCAL root; got {:?}",
            member.local_roots
        );
    }
}
