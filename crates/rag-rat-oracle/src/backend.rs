//! The LIVE backend registry: what distinguishes one resident language server from another once
//! the shared client substrate is in place.
//!
//! [`crate::manifest::ToolManifest`] answers "which binary, with what argv, and can it run here?"
//! for every backend, batch and live alike. This module answers the questions only a *live* driver
//! asks: which language's files belong on its worklist, what `languageId` to open them under, and
//! which readiness signal the server actually emits. Adding a backend is one entry here plus one
//! manifest entry — not new protocol code.

use std::path::{Path, PathBuf};

use rag_rat_base::language::Language;

use super::OracleTool;
use super::lsp::readiness::ReadinessPolicy;

/// The static registry entry for one live oracle backend.
#[derive(Debug, Clone, Copy)]
pub struct LiveBackend {
    /// The persisted tool id its verdicts are written under. Always a non-`batch_capable` tool.
    pub tool: OracleTool,
    /// The languages whose files this backend resolves. Drives the watcher's worklist filter, so a
    /// backend never sees a path it cannot open. Usually one, but a single server can own several:
    /// clangd serves C and C++ from one session.
    pub languages: &'static [Language],
    /// How this server announces that it is ready to answer definitions.
    pub(crate) readiness: ReadinessPolicy,
    /// LSP `languageId` per file extension, first match wins; the last entry is the fallback for
    /// any extension the language claims but this table doesn't name.
    language_ids: &'static [(&'static str, &'static str)],
    /// The file whose presence makes this server treat the checkout as a real PROJECT, and
    /// therefore the thing whose load it reports. `None` for a backend whose readiness is
    /// session-level and needs no project at all.
    ///
    /// This is the same file the backend's `prerequisite_blocked` gate looks for, because it is
    /// the same question: a checkout with no such project emits no readiness signal, so the
    /// backend could only ever sit in `Warming`.
    pub(crate) project_marker: Option<ProjectMarker>,
}

/// A backend's project marker, and how it relates to the documents it governs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProjectMarker {
    pub(crate) file: &'static str,
    scope: ProjectScope,
}

/// Where a project marker has to sit relative to a document for that document to belong to it.
/// The two live backends genuinely differ, and assuming either shape for the other misclassifies
/// ordinary layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectScope {
    /// The marker governs its own SUBTREE: a document belongs to it only when the marker sits in
    /// one of the document's ancestor directories. `tsconfig.json` works this way — it is the
    /// project definition, and it lives above the sources it declares.
    Enclosing,
    /// The marker is a build artifact the server discovers on its own, and need not be an ancestor
    /// of anything it governs: an out-of-tree CMake build puts `compile_commands.json` under
    /// `build/` while the sources sit in `src/`. Any document in the checkout qualifies once the
    /// marker exists ANYWHERE in it — measured, clangd emits a full load cycle and resolves across
    /// translation units for such a layout, and even for a file the database does not list at all
    /// (it infers flags from a sibling entry). Requiring the marker to be an ancestor would report
    /// that ordinary CMake project as `Blocked`.
    Checkout,
}

impl LiveBackend {
    /// The live backend for `tool`, or `None` for a batch tool. Total over the non-batch variants:
    /// [`OracleTool::batch_capable`] is `false` exactly when this returns `Some`.
    pub fn for_tool(tool: OracleTool) -> Option<Self> {
        match tool {
            OracleTool::RaLsp => Some(Self {
                tool,
                languages: &[Language::Rust],
                // rust-analyzer reports load/index quiescence explicitly, for any checkout.
                readiness: ReadinessPolicy::ServerStatus,
                language_ids: &[("rs", "rust")],
                project_marker: None,
            }),
            OracleTool::TsLsp => Some(Self {
                tool,
                languages: &[Language::TypeScript],
                // typescript-language-server has no quiescence notification. The only warm-up
                // signal it emits is the work-done progress cycle bracketing a project load —
                // which is why the manifest blocks this backend on a checkout with no
                // tsconfig.json, where no such cycle is ever emitted.
                readiness: ReadinessPolicy::WorkDoneProgress,
                language_ids: &[("tsx", "typescriptreact"), ("ts", "typescript")],
                project_marker: Some(ProjectMarker {
                    file: "tsconfig.json",
                    scope: ProjectScope::Enclosing,
                }),
            }),
            // clangd serves BOTH C and C++ from one session — the first backend whose language
            // set is not a singleton. Its background index is what resolves a call across
            // translation units; see the manifest entry for what that costs.
            OracleTool::ClangdLsp => Some(Self {
                tool,
                languages: &[Language::C, Language::Cpp],
                // clangd brackets its indexing in a work-done progress cycle and, like
                // typescript-language-server, emits nothing until a document is opened.
                readiness: ReadinessPolicy::WorkDoneProgress,
                // `.h` follows the language registry's default owner (C); a C++ target claims it
                // explicitly there, and clangd copes either way.
                language_ids: &[
                    ("cc", "cpp"),
                    ("cpp", "cpp"),
                    ("cxx", "cpp"),
                    ("c++", "cpp"),
                    ("hh", "cpp"),
                    ("hpp", "cpp"),
                    ("hxx", "cpp"),
                    ("h++", "cpp"),
                    ("h", "c"),
                    ("c", "c"),
                ],
                // clangd resolves ACROSS translation units only through its index, and it builds
                // that index from the compilation database. Without one it answers with the
                // header declaration and emits no progress at all (measured) — the same
                // no-signal state a tsconfig-less TypeScript checkout is in.
                project_marker: Some(ProjectMarker {
                    file: "compile_commands.json",
                    scope: ProjectScope::Checkout,
                }),
            }),
            OracleTool::RustAnalyzer
            | OracleTool::ScipClang
            | OracleTool::ScipPython
            | OracleTool::ScipTypescript
            | OracleTool::ScipJava => None,
        }
    }

    /// Every live backend, in [`OracleTool::ALL`] order — the watcher's enumeration seam.
    pub fn all() -> impl Iterator<Item = Self> {
        OracleTool::ALL.iter().copied().filter_map(Self::for_tool)
    }

    /// The LSP `languageId` to open `path` under. A mis-declared id makes a server reject or
    /// mis-parse the document (tsserver warns and rewrites, other servers simply refuse), so the
    /// extension decides rather than a per-backend constant.
    pub(crate) fn language_id_for(&self, path: &str) -> &'static str {
        let extension = path.rsplit('.').next().unwrap_or_default();
        self.language_ids
            .iter()
            .find_map(|(ext, id)| (*ext == extension).then_some(*id))
            .unwrap_or_else(|| self.language_ids.last().expect("a backend declares one id").1)
    }

    /// Whether this backend's language claims `path` — the worklist filter. Reuses the language
    /// registry's extension set so a backend and the indexer never disagree about which files
    /// exist in that language.
    pub fn claims_path(&self, path: &str) -> bool {
        let path = std::path::Path::new(path);
        self.languages.iter().any(|language| language.claims_path(path))
    }

    /// Whether this backend resolves `language` — the watcher's gate on the checkout's targets.
    pub fn resolves_language(&self, language: Language) -> bool {
        self.languages.contains(&language)
    }

    /// Whether opening `path` (repo-relative, under `root`) would produce an observable readiness
    /// signal — i.e. whether it is a useful warm-up document.
    ///
    /// `ServerStatus` backends report session-level quiescence regardless of what is open, so any
    /// file will do. `typescript-language-server` only brackets a TSCONFIG PROJECT's load in a
    /// progress cycle: opening a file that belongs to no project creates an inferred project
    /// SILENTLY (measured: no `$/progress` at all), so warming on one teaches the session nothing
    /// and it stays `Warming` while a file that IS in a project would have warmed it.
    pub(crate) fn open_signals_readiness(&self, root: &Path, path: &str) -> bool {
        // A document this backend cannot open teaches it nothing, whatever project contains it —
        // and the two live backends' project markers can coexist in one checkout, so the marker
        // alone would let a `.ts` file qualify as a clangd warm-up. Callers filter by language
        // already; asserting it here keeps the answer right for any caller.
        if !self.claims_path(path) {
            return false;
        }
        match self.readiness {
            ReadinessPolicy::ServerStatus => true,
            ReadinessPolicy::WorkDoneProgress =>
                self.project_marker.is_some_and(|marker| match marker.scope {
                    ProjectScope::Enclosing =>
                        enclosing_project_dir(root, &root.join(path), marker.file).is_some(),
                    // One database: the session is pointed at it, so any document works.
                    // Several: nothing is pointed anywhere, so a document counts only if the
                    // server would find a database for it on its own.
                    ProjectScope::Checkout => match marker_dirs(root, marker.file).as_slice() {
                        [] => false,
                        [_only] => true,
                        _ => discoverable_marker_dir(root, &root.join(path), marker.file).is_some(),
                    },
                }),
        }
    }

    /// Any document in the checkout whose `didOpen` would produce an observable readiness signal,
    /// found by search rather than taken from the worklist.
    ///
    /// This is what makes the backend usable on a checkout whose CHANGED files all sit outside a
    /// project: the expensive warm-up is per SERVER, not per project, so loading one real project
    /// makes the session ready — after which even project-less files resolve correctly (measured).
    /// Without it, a worklist of only project-less files would re-open an ineffective document
    /// every pass and never warm.
    ///
    /// `None` means the checkout contains no project this backend could ever warm on, which is
    /// also what makes its `prerequisite_blocked` gate fire — the two answer the same question, so
    /// they cannot disagree.
    pub(crate) fn warmup_document(&self, root: &Path) -> Option<PathBuf> {
        match self.readiness {
            // Session-level quiescence needs no document.
            ReadinessPolicy::ServerStatus => None,
            ReadinessPolicy::WorkDoneProgress =>
                self.project_marker.and_then(|marker| match marker.scope {
                    ProjectScope::Enclosing =>
                        find_document_in_project(root, self.languages, marker.file, false),
                    // The marker can sit anywhere, so the two halves are searched independently
                    // and a document only counts once the marker has been found somewhere.
                    ProjectScope::Checkout => match marker_dirs(root, marker.file).as_slice() {
                        [] => None,
                        [_only] => find_any_document(root, self.languages),
                        // With several databases the warm-up must pick a document the server can
                        // actually configure, or it opens one that yields no load cycle. Note this
                        // SEARCHES for such a document: filtering the first candidate would let a
                        // single stray file at the root declare the whole checkout unwarmable.
                        _ => find_document_where(root, self.languages, &|document| {
                            discoverable_marker_dir(root, document, marker.file).is_some()
                        }),
                    },
                }),
        }
    }

    /// The argv this backend's server is spawned with for `root`: the manifest's static arguments
    /// plus any that depend on the checkout.
    ///
    /// clangd is the reason this is not just the static list. It discovers a compilation database
    /// only in an opened file's ancestor directories and their `build/` subdirectory, so a
    /// database anywhere else — `out/`, `cmake-build-debug/`, any project-specific name — is
    /// invisible to it. Passing the directory we found makes every layout behave the same, and
    /// keeps the prerequisite gate honest: it accepts a database wherever it sits precisely
    /// because the session is then told where that is.
    pub(crate) fn spawn_args(&self, static_args: &[&'static str], root: &Path) -> Vec<String> {
        let mut args: Vec<String> = static_args.iter().map(|arg| (*arg).to_string()).collect();
        if let Some(marker) = self.project_marker
            && marker.scope == ProjectScope::Checkout
            && let [only] = marker_dirs(root, marker.file).as_slice()
        {
            args.push(format!("--compile-commands-dir={}", only.display()));
        }
        args
    }

    /// Whether this checkout can ever produce a readiness signal for this backend. Backs the
    /// manifest's prerequisite gate.
    pub fn checkout_can_signal_readiness(&self, root: &Path) -> bool {
        match self.readiness {
            ReadinessPolicy::ServerStatus => true,
            ReadinessPolicy::WorkDoneProgress => self.warmup_document(root).is_some(),
        }
    }
}

/// Directories never searched for a warm-up DOCUMENT: `node_modules` vendors dependency sources,
/// and a dot-directory is VCS/tooling state (including the `.cache/clangd` index clangd writes
/// itself) — never somewhere to pick a document this checkout owns.
fn is_searchable_dir(name: &str) -> bool {
    name != "node_modules" && !name.starts_with('.')
}

/// Directories never searched for a project MARKER. Deliberately more permissive than
/// [`is_searchable_dir`]: a build directory may legitimately be hidden (`.build/`), and a
/// compilation database there is as real as one in `build/`. Only trees that cannot hold this
/// checkout's own database are excluded — VCS internals, rag-rat's own state, clangd's index, and
/// vendored dependencies.
fn is_searchable_for_marker(name: &str) -> bool {
    !matches!(name, "node_modules" | ".git" | ".rag-rat" | ".cache" | ".hg" | ".svn")
}

/// The directory of the nearest `marker` file at or above `path`, stopping at `root`. `None` means
/// no project governs the file at all, and opening it produces NO project-load progress.
///
/// ANCESTRY IS THE WHOLE TEST — deliberately. A config's `files`/`include`/`exclude` decide
/// membership, so an ancestor config does not prove the file is *in* that project; the tempting
/// "fix" is to evaluate those globs here. Measured against the real server, that would be wasted
/// complexity: opening a file whose ancestor config EXCLUDES it (root `"include": ["src"]`,
/// opening `scripts/main.ts`) still emits a full `$/progress` begin/end cycle, because tsserver
/// loads the config project in order to decide the file isn't in it. The load is observable either
/// way. Only a file with no ancestor config at all loads silently. Do not reimplement tsconfig
/// glob semantics here — it would add a second, subtler source of truth for no gain.
fn enclosing_project_dir(root: &Path, path: &Path, marker: &str) -> Option<PathBuf> {
    let mut dir = path.parent()?;
    loop {
        if dir.join(marker).exists() {
            return Some(dir.to_path_buf());
        }
        if dir == root {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// The directory holding `marker`, searched anywhere at or below `root` — the
/// [`ProjectScope::Checkout`] lookup. Returns the DIRECTORY, not a bool, because the server has to
/// be told where it is: clangd searches only an opened file's ancestors and their `build/`
/// subdirectory, so a database in `out/` or `cmake-build-debug/` is invisible to it without
/// `--compile-commands-dir` (measured: no progress at all, and calls resolve to header
/// declarations). Accepting a checkout whose database the server cannot find would report it
/// usable while it silently never warms.
fn marker_dirs(root: &Path, marker: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect_marker_dirs(root, marker, &mut found);
    found
}

/// Collect marker directories, stopping at two — the only distinction any caller draws is
/// "exactly one" versus "several", and a monorepo can hold hundreds.
fn collect_marker_dirs(dir: &Path, marker: &str, found: &mut Vec<PathBuf>) {
    if found.len() >= 2 {
        return;
    }
    if dir.join(marker).exists() {
        found.push(dir.to_path_buf());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirectories: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            is_searchable_for_marker(&entry.file_name().to_string_lossy())
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| entry.path())
        .collect();
    subdirectories.sort();
    for sub in subdirectories {
        collect_marker_dirs(&sub, marker, found);
    }
}

/// The marker directory the SERVER would find for `path` on its own — clangd searches an opened
/// file's ancestor directories and a `build/` subdirectory of each. Used when the checkout holds
/// several databases and the session therefore points at none.
fn discoverable_marker_dir(root: &Path, path: &Path, marker: &str) -> Option<PathBuf> {
    let mut dir = path.parent()?;
    loop {
        for candidate in [dir.to_path_buf(), dir.join("build")] {
            if candidate.join(marker).exists() {
                return Some(candidate);
            }
        }
        if dir == root {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// The first file one of `languages` claims at or below `dir`, regardless of any project marker —
/// the single-database [`ProjectScope::Checkout`] warm-up document.
fn find_any_document(dir: &Path, languages: &[Language]) -> Option<PathBuf> {
    find_document_where(dir, languages, &|_| true)
}

/// The first file one of `languages` claims at or below `dir` that also satisfies `accept`.
fn find_document_where(
    dir: &Path,
    languages: &[Language],
    accept: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut subdirectories = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if !is_searchable_dir(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            subdirectories.push(path);
        } else if languages.iter().any(|language| language.claims_path(&path)) && accept(&path) {
            return Some(path);
        }
    }
    subdirectories.sort();
    subdirectories.into_iter().find_map(|sub| find_document_where(&sub, languages, accept))
}

/// The first file one of `languages` claims that lies inside a `marker` project at or below `dir`.
///
/// Deliberately UNBOUNDED in depth (it only skips vendored/VCS directories): a project can sit
/// arbitrarily deep in a monorepo — `services/teams/foo/web/tsconfig.json` is an ordinary layout —
/// and a fixed depth limit would silently disable those checkouts. The walk early-exits on the
/// first hit, so the only full traversal is the case that ends in a `Blocked` verdict, which the
/// watcher then backs off to five-minute retries.
fn find_document_in_project(
    dir: &Path,
    languages: &[Language],
    marker: &str,
    inside_project: bool,
) -> Option<PathBuf> {
    let inside_project = inside_project || dir.join(marker).exists();
    let mut subdirectories = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        if !is_searchable_dir(&name.to_string_lossy()) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            subdirectories.push(entry.path());
        } else if inside_project
            && languages.iter().any(|language| language.claims_path(&entry.path()))
        {
            return Some(entry.path());
        }
    }
    subdirectories
        .into_iter()
        .find_map(|sub| find_document_in_project(&sub, languages, marker, inside_project))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_backends_are_exactly_the_non_batch_tools() {
        // The two must stay in lockstep: a live tool with no backend entry would be enumerated by
        // the watcher and spawn nothing, and a backend for a batch tool would double-write edges
        // the batch pass already owns authoritatively.
        for &tool in OracleTool::ALL {
            assert_eq!(
                LiveBackend::for_tool(tool).is_some(),
                !tool.batch_capable(),
                "{} disagrees about being a live backend",
                tool.as_db_str()
            );
        }
    }

    #[test]
    fn every_live_backend_copies_monikers_from_a_batch_tool_for_its_own_language() {
        // A live verdict's `scip_symbol` is its batch counterpart's moniker verbatim. If the two
        // resolved different languages the copy would be meaningless, so the pairing is asserted
        // rather than assumed.
        for backend in LiveBackend::all() {
            let source = backend
                .tool
                .batch_moniker_source()
                .unwrap_or_else(|| panic!("{} has no moniker source", backend.tool.as_db_str()));
            assert!(source.batch_capable(), "a moniker source must be a batch tool");
            let batch_languages = crate::ToolManifest::for_tool(source).languages;
            for language in backend.languages {
                assert!(
                    batch_languages.contains(&language.as_str()),
                    "{} resolves {language} but copies monikers from {}, which indexes \
                     {batch_languages:?}",
                    backend.tool.as_db_str(),
                    source.as_db_str(),
                );
            }
        }
    }

    #[test]
    fn every_live_backend_declares_ids_for_the_extensions_its_language_claims() {
        // `claims_path` admits a file to the worklist and `language_id_for` decides how it is
        // opened; a gap between them means a file gets resolved under a fallback id.
        for backend in LiveBackend::all() {
            for extension in backend.languages.iter().flat_map(|l| l.target_extensions()) {
                let path = format!("src/file.{extension}");
                assert!(backend.claims_path(&path), "{path} must be claimed");
                assert!(
                    backend.language_ids.iter().any(|(ext, _)| ext == extension),
                    "{} claims .{extension} but declares no languageId for it",
                    backend.tool.as_db_str(),
                );
            }
        }
    }

    #[test]
    fn typescript_opens_tsx_as_typescriptreact() {
        let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
        assert_eq!(ts.language_id_for("src/main.ts"), "typescript");
        assert_eq!(ts.language_id_for("src/App.tsx"), "typescriptreact");
        // An extension the table doesn't name still opens under the backend's fallback rather
        // than an empty id the server would reject.
        assert_eq!(ts.language_id_for("src/no-extension"), "typescript");
        assert!(!ts.claims_path("src/lib.rs"), "another language's file never enters the worklist");
    }

    /// A TypeScript project at `relative_dir` holding one `main.ts`.
    fn write_project(root: &Path, relative_dir: &str) {
        let dir = root.join(relative_dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        std::fs::write(dir.join("main.ts"), "export function greet() {}\n").unwrap();
    }

    #[test]
    fn enclosing_tsconfig_walks_up_to_the_nearest_project_and_stops_at_the_root() {
        // This is how tsserver assigns a file to a project, and it decides whether opening the
        // file produces an observable load. A file under no project opens as an inferred project
        // SILENTLY, so warming on it teaches the session nothing.
        let dir = rag_rat_base::test_scratch::ScratchDir::new("ts-lsp-enclosing");
        std::fs::create_dir_all(dir.join("packages/app/src")).unwrap();
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("packages/app/tsconfig.json"), "{}").unwrap();

        assert_eq!(
            enclosing_project_dir(&dir, &dir.join("packages/app/src/main.ts"), "tsconfig.json"),
            Some(dir.join("packages/app")),
            "the nearest enclosing project wins",
        );
        assert_eq!(
            enclosing_project_dir(&dir, &dir.join("scripts/tool.ts"), "tsconfig.json"),
            None,
            "a file under no project has none",
        );
    }

    #[test]
    fn a_warmup_document_is_found_at_any_depth_and_only_inside_a_project() {
        // A project can sit arbitrarily deep in a monorepo; a depth limit would silently disable
        // those checkouts entirely, which is worse than the walk it saves.
        let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
        let dir = rag_rat_base::test_scratch::ScratchDir::new("ts-lsp-warmup-doc");
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/tool.ts"), "export function x() {}\n").unwrap();
        assert_eq!(
            ts.warmup_document(&dir),
            None,
            "a TypeScript file outside every project is not a warm-up document",
        );
        assert!(!ts.checkout_can_signal_readiness(&dir));

        write_project(&dir, "services/teams/foo/web");
        assert_eq!(
            ts.warmup_document(&dir),
            Some(dir.join("services/teams/foo/web/main.ts")),
            "a deeply nested project is still found",
        );
        assert!(ts.checkout_can_signal_readiness(&dir));
    }

    #[test]
    fn the_warmup_search_ignores_vendored_and_vcs_directories() {
        // `node_modules` ships thousands of tsconfigs describing DEPENDENCIES. Warming on one
        // would report the checkout usable while none of ITS files ever resolve.
        let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
        let dir = rag_rat_base::test_scratch::ScratchDir::new("ts-lsp-vendored-warmup");
        write_project(&dir, "node_modules/some-dep");
        write_project(&dir, ".cache/tooling");
        assert_eq!(ts.warmup_document(&dir), None);
        assert!(!ts.checkout_can_signal_readiness(&dir));
    }

    #[test]
    fn a_server_status_backend_needs_no_warmup_document_and_is_never_blocked_on_one() {
        // rust-analyzer reports quiescence for any checkout, so the whole notion is TS-specific
        // and must not leak into the other backend's gating.
        let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
        let dir = rag_rat_base::test_scratch::ScratchDir::new("ra-lsp-warmup-doc");
        assert_eq!(rust.warmup_document(&dir), None);
        assert!(rust.checkout_can_signal_readiness(&dir), "an empty checkout still signals");
        assert!(rust.open_signals_readiness(&dir, "src/lib.rs"), "any document will do");
        assert!(rust.project_marker.is_none(), "session-level readiness needs no project");
    }

    #[test]
    fn clangd_serves_c_and_cpp_from_one_backend() {
        // The first backend whose language set is not a singleton. Both languages must reach its
        // worklist, or half its files would silently never be resolved.
        let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
        assert!(clangd.resolves_language(Language::C));
        assert!(clangd.resolves_language(Language::Cpp));
        assert!(!clangd.resolves_language(Language::Rust));
        for path in ["src/a.c", "src/a.h", "src/a.cpp", "src/a.cc", "src/a.hpp"] {
            assert!(clangd.claims_path(path), "{path} must be claimed");
        }
        assert!(!clangd.claims_path("src/a.rs"));
        assert!(!clangd.claims_path("src/a.ts"));
    }

    #[test]
    fn clangd_opens_each_dialect_under_its_own_language_id() {
        // A C++ file opened as `c` parses under the wrong dialect, so the extension decides.
        // `.h` follows the language registry's default owner (C), which clangd copes with.
        let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
        assert_eq!(clangd.language_id_for("src/a.c"), "c");
        assert_eq!(clangd.language_id_for("src/a.h"), "c");
        for path in ["src/a.cc", "src/a.cpp", "src/a.cxx", "src/a.hpp", "src/a.hh"] {
            assert_eq!(clangd.language_id_for(path), "cpp", "{path}");
        }
    }

    #[test]
    fn a_backends_project_marker_is_the_file_its_prerequisite_looks_for() {
        // The warm-up search and the prerequisite gate must ask the SAME question, or a checkout
        // could pass the gate and still have nothing to warm on (or vice versa).
        let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-marker");
        let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
        assert_eq!(clangd.project_marker.map(|m| m.file), Some("compile_commands.json"));
        assert!(!clangd.checkout_can_signal_readiness(&dir), "no compdb ⇒ no signal possible");

        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
        assert!(!clangd.checkout_can_signal_readiness(&dir), "sources alone are not a project");
        std::fs::write(dir.join("compile_commands.json"), "[]").unwrap();
        assert_eq!(clangd.warmup_document(&dir), Some(dir.join("src/main.c")));
        assert!(clangd.checkout_can_signal_readiness(&dir));
        // The two live backends' project markers can coexist in one checkout, so a document
        // qualifies only if THIS backend could open it — not merely because a project contains it.
        assert!(clangd.open_signals_readiness(&dir, "src/main.c"));
        assert!(
            !clangd.open_signals_readiness(&dir, "src/app.ts"),
            "another language's file is not a clangd warm-up document, project or not",
        );
    }

    #[test]
    fn an_out_of_tree_compilation_database_still_counts_as_a_project() {
        // A tsconfig DECLARES the sources beneath it; a compile_commands.json is a build artifact
        // that need not sit above anything. The standard out-of-tree CMake layout puts it under
        // `build/` with the sources in `src/` — measured, clangd resolves across translation units
        // there just fine, so requiring the marker to be an ancestor would report an ordinary
        // CMake project as Blocked.
        let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
        let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-out-of-tree");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::write(dir.join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
        assert!(!clangd.checkout_can_signal_readiness(&dir), "sources with no compdb anywhere");

        std::fs::write(dir.join("build/compile_commands.json"), "[]").unwrap();
        assert!(
            clangd.checkout_can_signal_readiness(&dir),
            "a compdb ANYWHERE in the checkout makes the backend usable",
        );
        assert_eq!(clangd.warmup_document(&dir), Some(dir.join("src/main.c")));
        assert!(
            clangd.open_signals_readiness(&dir, "src/main.c"),
            "a source with no ancestor compdb still warms clangd",
        );
    }

    #[test]
    fn clangd_is_told_where_a_compilation_database_it_could_not_find_lives() {
        // clangd searches only an opened file's ancestors and their `build/` subdirectory.
        // Measured: with the database in `out/` and no flag it emits no progress at all and
        // resolves calls to header declarations; with `--compile-commands-dir` it resolves across
        // translation units. Accepting the checkout is only honest because we pass the directory.
        let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
        let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-compdb-dir");
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out/compile_commands.json"), "[]").unwrap();

        let args = clangd.spawn_args(&["--background-index"], &dir);
        assert_eq!(args[0], "--background-index", "the static argv comes first");
        assert!(
            args.contains(&format!("--compile-commands-dir={}", dir.join("out").display())),
            "the discovered database directory must be passed: {args:?}",
        );
    }

    #[test]
    fn a_hidden_build_directory_still_counts_as_a_compilation_database() {
        // A build directory may legitimately be hidden (`.build/`), and a database there is as
        // real as one in `build/`. The DOCUMENT search still skips dot-directories — those hold
        // tooling state, not this checkout's sources — so the two searches differ on purpose.
        let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
        let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-hidden-build");
        std::fs::create_dir_all(dir.join(".build")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(".build/compile_commands.json"), "[]").unwrap();
        std::fs::write(dir.join("src/main.c"), "int main(void){return 0;}\n").unwrap();

        assert!(clangd.checkout_can_signal_readiness(&dir));
        assert!(
            clangd
                .spawn_args(&["--background-index"], &dir)
                .contains(&format!("--compile-commands-dir={}", dir.join(".build").display())),
        );
        // The warm-up document still comes from the visible tree.
        assert_eq!(clangd.warmup_document(&dir), Some(dir.join("src/main.c")));
    }

    #[test]
    fn a_vendored_or_vcs_database_is_never_mistaken_for_the_checkouts_own() {
        // Counting a stray database would be worse than missing one: it would flip a working
        // single-database checkout into the multi-database mode and drop the flag that makes it
        // resolvable.
        let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
        let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-vendored-db");
        std::fs::create_dir_all(dir.join("node_modules/dep")).unwrap();
        std::fs::create_dir_all(dir.join(".git/weird")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("node_modules/dep/compile_commands.json"), "[]").unwrap();
        std::fs::write(dir.join(".git/weird/compile_commands.json"), "[]").unwrap();
        std::fs::write(dir.join("compile_commands.json"), "[]").unwrap();
        std::fs::write(dir.join("src/main.c"), "int main(void){return 0;}\n").unwrap();

        assert!(
            clangd
                .spawn_args(&["--background-index"], &dir)
                .contains(&format!("--compile-commands-dir={}", dir.display())),
            "the checkout's own database is still the single unambiguous one",
        );
    }

    #[test]
    fn several_compilation_databases_are_left_to_the_servers_own_per_file_lookup() {
        // `--compile-commands-dir` is GLOBAL: it overrides clangd's per-file search for every
        // document. With one database that is exactly right; with several it would force one
        // project's flags onto another's files, and wrong `-D`/include flags select a different
        // `#ifdef` branch — a wrong definition, persisted. So the flag is only passed when it is
        // unambiguous, and otherwise clangd's own per-file lookup (ancestors and their `build/`)
        // decides, which is correct by construction.
        let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
        let dir = rag_rat_base::test_scratch::ScratchDir::new("clangd-multi-db");
        for project in ["proj-a", "proj-b"] {
            std::fs::create_dir_all(dir.join(project).join("build")).unwrap();
            std::fs::write(dir.join(project).join("build/compile_commands.json"), "[]").unwrap();
            std::fs::write(dir.join(project).join("main.c"), "int main(void){return 0;}\n")
                .unwrap();
        }
        assert_eq!(
            clangd.spawn_args(&["--background-index"], &dir),
            vec!["--background-index".to_string()],
            "no database may be forced globally when several exist",
        );
        // Each project's own file is still fine: clangd finds `<dir>/build/` beside it.
        assert!(clangd.open_signals_readiness(&dir, "proj-a/main.c"));
        // A file belonging to no project is not a usable warm-up document here, because nothing
        // points the session at a database on its behalf.
        std::fs::write(dir.join("stray.c"), "int stray(void){return 0;}\n").unwrap();
        assert!(!clangd.open_signals_readiness(&dir, "stray.c"));
        assert!(
            clangd.checkout_can_signal_readiness(&dir),
            "the per-project files remain warmable, so the backend is not blocked",
        );
    }

    #[test]
    fn a_backend_with_no_checkout_scoped_marker_gets_only_its_static_argv() {
        // The dynamic argument is clangd-shaped; the other backends must not acquire a stray flag
        // their server would reject.
        let dir = rag_rat_base::test_scratch::ScratchDir::new("static-argv");
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
        assert_eq!(ts.spawn_args(&["--stdio"], &dir), vec!["--stdio".to_string()]);
        let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
        assert!(rust.spawn_args(&[], &dir).is_empty());
        // And with no database anywhere, clangd gets no directory to point at either.
        let empty = rag_rat_base::test_scratch::ScratchDir::new("static-argv-empty");
        let clangd = LiveBackend::for_tool(OracleTool::ClangdLsp).unwrap();
        assert_eq!(clangd.spawn_args(&["--background-index"], &empty), vec![
            "--background-index".to_string()
        ],);
    }

    #[test]
    fn a_typescript_project_still_has_to_enclose_its_documents() {
        // The other scope, asserted alongside so the two cannot be conflated: a tsconfig sibling
        // of the sources governs nothing, because tsserver resolves a file's project by walking UP
        // from the file.
        let ts = LiveBackend::for_tool(OracleTool::TsLsp).unwrap();
        let dir = rag_rat_base::test_scratch::ScratchDir::new("ts-sibling-config");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(dir.join("src/main.ts"), "export const x = 1;\n").unwrap();
        std::fs::write(dir.join("config/tsconfig.json"), "{}").unwrap();
        assert!(
            !ts.open_signals_readiness(&dir, "src/main.ts"),
            "a config in a SIBLING directory governs nothing under src/",
        );
        assert_eq!(ts.warmup_document(&dir), None, "and there is nothing to warm on");
    }

    #[test]
    fn rust_claims_only_rust_paths() {
        let rust = LiveBackend::for_tool(OracleTool::RaLsp).unwrap();
        assert_eq!(rust.language_id_for("src/lib.rs"), "rust");
        assert!(rust.claims_path("src/lib.rs"));
        assert!(!rust.claims_path("src/main.ts"));
        assert!(!rust.claims_path("Cargo.toml"));
    }
}
