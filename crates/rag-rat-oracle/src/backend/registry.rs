//! The per-tool registry entry itself: which languages a live backend owns, how it opens their
//! files, which project marker it needs, and every question the live driver asks of one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rag_rat_base::language::Language;

use super::documents;
use super::layout::{ProjectLayout, marker_sites};
use crate::OracleTool;
use crate::lsp::readiness::ReadinessPolicy;

/// The static registry entry for one live oracle backend.
#[derive(Debug, Clone, Copy)]
pub struct LiveBackend {
    /// The persisted tool id its verdicts are written under. Always a non-`batch_capable` tool.
    pub tool: OracleTool,
    /// The languages whose files this backend resolves. Drives the watcher's worklist filter, so a
    /// backend never sees a path it cannot open. Usually one, but a single server can own several:
    /// clangd serves C and C++ from one session.
    pub languages: &'static [Language],
    /// How this backend is named in operator-facing text. Written out rather than derived from
    /// `languages`, whose entries are the lowercase registry tokens (`c`, `cpp`) — joining those
    /// renders "the live c/cpp oracle" in a hint a human reads.
    pub(crate) display_name: &'static str,
    /// How this server announces that it is ready to answer definitions.
    pub(crate) readiness: ReadinessPolicy,
    /// LSP `languageId` per file extension, first match wins; the last entry is the fallback for
    /// any extension the language claims but this table doesn't name.
    pub(super) language_ids: &'static [(&'static str, &'static str)],
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
    /// What the operator loses while this marker is missing, and what to do about it — the tail of
    /// the prerequisite hint. The two progress-signalled backends share the gate and the sentence
    /// that states it, but the symptom is measured per server and the remedy is a different
    /// command, so each carries its own text here rather than the hint describing both vaguely.
    pub(crate) hint_detail: &'static str,
}

/// Where a project marker has to sit relative to a document for that document to belong to it.
/// The two live backends genuinely differ, and assuming either shape for the other misclassifies
/// ordinary layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProjectScope {
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
                display_name: "Rust",
                // rust-analyzer reports load/index quiescence explicitly, for any checkout.
                readiness: ReadinessPolicy::ServerStatus,
                language_ids: &[("rs", "rust")],
                project_marker: None,
            }),
            OracleTool::TsLsp => Some(Self {
                tool,
                languages: &[Language::TypeScript],
                display_name: "TypeScript",
                // typescript-language-server has no quiescence notification. The only warm-up
                // signal it emits is the work-done progress cycle bracketing a project load —
                // which is why the manifest blocks this backend on a checkout with no
                // tsconfig.json, where no such cycle is ever emitted.
                readiness: ReadinessPolicy::WorkDoneProgress,
                language_ids: &[("tsx", "typescriptreact"), ("ts", "typescript")],
                project_marker: Some(ProjectMarker {
                    file: "tsconfig.json",
                    scope: ProjectScope::Enclosing,
                    hint_detail: "Asked mid-load, typescript-language-server resolves an imported \
                                  callee to the import statement instead of the definition. Add a \
                                  tsconfig.json for the project (most TypeScript projects ship \
                                  one) to enable it.",
                }),
            }),
            // clangd serves BOTH C and C++ from one session — the first backend whose language
            // set is not a singleton. Its background index is what resolves a call across
            // translation units; see the manifest entry for what that costs.
            OracleTool::ClangdLsp => Some(Self {
                tool,
                languages: &[Language::C, Language::Cpp],
                display_name: "C/C++",
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
                    hint_detail: "Without a compilation database clangd resolves a call into \
                                  another translation unit only to the callee's header \
                                  declaration, and emits no project-load progress at all. \
                                  Generate one (e.g. `bear -- make`, CMake \
                                  `-DCMAKE_EXPORT_COMPILE_COMMANDS=ON`) to enable it.",
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

    /// Resolve this checkout's project layout — the one filesystem scan a session performs.
    pub fn resolve_layout(&self, root: &Path) -> ProjectLayout {
        let Some(marker) = self.project_marker else {
            return ProjectLayout::default();
        };
        match marker.scope {
            // An enclosing-scoped marker is answered per document by walking UP, which is cheap
            // and needs no precomputed set.
            ProjectScope::Enclosing => ProjectLayout::default(),
            ProjectScope::Checkout =>
                ProjectLayout::from_marker_sites(marker.file, marker_sites(root, marker.file)),
        }
    }

    /// Whether opening `path` (repo-relative, under `root`) would produce an observable readiness
    /// signal — i.e. whether it is a useful warm-up document.
    ///
    /// `ServerStatus` backends report session-level quiescence regardless of what is open, so any
    /// file will do. `typescript-language-server` only brackets a TSCONFIG PROJECT's load in a
    /// progress cycle: opening a file that belongs to no project creates an inferred project
    /// SILENTLY (measured: no `$/progress` at all), so warming on one teaches the session nothing
    /// and it stays `Warming` while a file that IS in a project would have warmed it.
    pub(crate) fn open_signals_readiness(
        &self,
        root: &Path,
        path: &str,
        layout: &ProjectLayout,
    ) -> bool {
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
                        documents::enclosing_project_dir(root, &root.join(path), marker.file)
                            .is_some(),
                    ProjectScope::Checkout =>
                        self.session_resolves(root, &root.join(path), layout, marker),
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
    pub(crate) fn warmup_document(&self, root: &Path, layout: &ProjectLayout) -> Option<PathBuf> {
        match self.readiness {
            // Session-level quiescence needs no document.
            ReadinessPolicy::ServerStatus => None,
            ReadinessPolicy::WorkDoneProgress =>
                self.project_marker.and_then(|marker| match marker.scope {
                    ProjectScope::Enclosing => documents::find_document_in_project(
                        root,
                        self.languages,
                        marker.file,
                        false,
                    ),
                    // The marker can sit anywhere, so the two halves are searched independently
                    // and a document only counts once the marker has been found somewhere.
                    // The warm-up must pick a document the session can actually configure, or it
                    // opens one that yields no load cycle. This SEARCHES for such a document:
                    // filtering the first candidate would let a single stray file at the root
                    // declare the whole checkout unwarmable.
                    ProjectScope::Checkout if layout.is_empty() => None,
                    ProjectScope::Checkout =>
                        documents::find_document_where(root, self.languages, &|document| {
                            self.session_resolves(root, document, layout, marker)
                        }),
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
    pub(crate) fn spawn_args(
        &self,
        static_args: &[&'static str],
        layout: &ProjectLayout,
    ) -> Vec<OsString> {
        let mut args: Vec<OsString> = static_args.iter().map(OsString::from).collect();
        if let Some(dir) = layout.sole_marker_dir() {
            // Built as an OsString, never through `Path::display()`: on Unix a path is bytes, and
            // formatting a non-UTF-8 component would substitute replacement characters and hand
            // the server a directory that does not exist.
            let mut arg = OsString::from("--compile-commands-dir=");
            arg.push(dir.as_os_str());
            args.push(arg);
        }
        args
    }

    /// Whether a document at `absolute` is one THIS SESSION can resolve correctly.
    ///
    /// With a single database the session is pointed at it, so every document is configured. With
    /// several, the session points at none and the server falls back to its own lookup — a
    /// document it cannot find a database for gets heuristic flags, and measured, that resolves a
    /// cross-translation-unit call to the callee's HEADER DECLARATION. Persisting that is a wrong
    /// verdict, not a missing one, so such documents are skipped rather than resolved.
    fn session_resolves(
        &self,
        root: &Path,
        absolute: &Path,
        layout: &ProjectLayout,
        marker: ProjectMarker,
    ) -> bool {
        if layout.is_empty() {
            return false;
        }
        layout.sole_marker_dir().is_some()
            || layout.discoverable_marker_dir(root, absolute, marker.file).is_some()
    }

    /// Whether this session can resolve `path` (repo-relative) — the live pass's per-file gate.
    pub fn session_can_resolve(&self, root: &Path, path: &str, layout: &ProjectLayout) -> bool {
        match self.project_marker {
            Some(marker) if marker.scope == ProjectScope::Checkout =>
                self.session_resolves(root, &root.join(path), layout, marker),
            _ => true,
        }
    }

    /// Whether this checkout can ever produce a readiness signal for this backend. Backs the
    /// manifest's prerequisite gate.
    pub fn checkout_can_signal_readiness(&self, root: &Path, layout: &ProjectLayout) -> bool {
        match self.readiness {
            ReadinessPolicy::ServerStatus => true,
            ReadinessPolicy::WorkDoneProgress => self.warmup_document(root, layout).is_some(),
        }
    }
}
