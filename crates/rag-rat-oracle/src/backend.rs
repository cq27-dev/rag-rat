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
    /// The language whose files this backend resolves. Drives the watcher's worklist filter, so a
    /// backend never sees a path it cannot open.
    pub language: Language,
    /// How this server announces that it is ready to answer definitions.
    pub(crate) readiness: ReadinessPolicy,
    /// LSP `languageId` per file extension, first match wins; the last entry is the fallback for
    /// any extension the language claims but this table doesn't name.
    language_ids: &'static [(&'static str, &'static str)],
}

impl LiveBackend {
    /// The live backend for `tool`, or `None` for a batch tool. Total over the non-batch variants:
    /// [`OracleTool::batch_capable`] is `false` exactly when this returns `Some`.
    pub fn for_tool(tool: OracleTool) -> Option<Self> {
        match tool {
            OracleTool::RaLsp => Some(Self {
                tool,
                language: Language::Rust,
                // rust-analyzer reports load/index quiescence explicitly, for any checkout.
                readiness: ReadinessPolicy::ServerStatus,
                language_ids: &[("rs", "rust")],
            }),
            OracleTool::TsLsp => Some(Self {
                tool,
                language: Language::TypeScript,
                // typescript-language-server has no quiescence notification. The only warm-up
                // signal it emits is the work-done progress cycle bracketing a project load —
                // which is why the manifest blocks this backend on a checkout with no
                // tsconfig.json, where no such cycle is ever emitted.
                readiness: ReadinessPolicy::WorkDoneProgress,
                language_ids: &[("tsx", "typescriptreact"), ("ts", "typescript")],
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
        self.language.claims_path(std::path::Path::new(path))
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
        match self.readiness {
            ReadinessPolicy::ServerStatus => true,
            ReadinessPolicy::WorkDoneProgress =>
                enclosing_tsconfig_dir(root, &root.join(path)).is_some(),
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
                find_document_in_project(root, self.language, false),
        }
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

/// Directories that never hold a project this checkout owns: `node_modules` vendors thousands of
/// dependency tsconfigs, and dot-directories are VCS/tooling state.
fn is_searchable_dir(name: &str) -> bool {
    name != "node_modules" && !name.starts_with('.')
}

/// The directory of the nearest `tsconfig.json` at or above `path`, stopping at `root` — how
/// tsserver decides which project a file belongs to. `None` means the file would open as an
/// inferred project, which loads SILENTLY (no progress cycle).
fn enclosing_tsconfig_dir(root: &Path, path: &Path) -> Option<PathBuf> {
    let mut dir = path.parent()?;
    loop {
        if dir.join("tsconfig.json").exists() {
            return Some(dir.to_path_buf());
        }
        if dir == root {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// The first file `language` claims that lies inside a tsconfig project at or below `dir`.
///
/// Deliberately UNBOUNDED in depth (it only skips vendored/VCS directories): a project can sit
/// arbitrarily deep in a monorepo — `services/teams/foo/web/tsconfig.json` is an ordinary layout —
/// and a fixed depth limit would silently disable those checkouts. The walk early-exits on the
/// first hit, so the only full traversal is the case that ends in a `Blocked` verdict, which the
/// watcher then backs off to five-minute retries.
fn find_document_in_project(
    dir: &Path,
    language: Language,
    inside_project: bool,
) -> Option<PathBuf> {
    let inside_project = inside_project || dir.join("tsconfig.json").exists();
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
        } else if inside_project && language.claims_path(&entry.path()) {
            return Some(entry.path());
        }
    }
    subdirectories
        .into_iter()
        .find_map(|sub| find_document_in_project(&sub, language, inside_project))
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
            assert!(
                batch_languages.contains(&backend.language.as_str()),
                "{} resolves {} but copies monikers from {}, which indexes {batch_languages:?}",
                backend.tool.as_db_str(),
                backend.language,
                source.as_db_str(),
            );
        }
    }

    #[test]
    fn every_live_backend_declares_ids_for_the_extensions_its_language_claims() {
        // `claims_path` admits a file to the worklist and `language_id_for` decides how it is
        // opened; a gap between them means a file gets resolved under a fallback id.
        for backend in LiveBackend::all() {
            for extension in backend.language.target_extensions() {
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
            enclosing_tsconfig_dir(&dir, &dir.join("packages/app/src/main.ts")),
            Some(dir.join("packages/app")),
            "the nearest enclosing project wins",
        );
        assert_eq!(
            enclosing_tsconfig_dir(&dir, &dir.join("scripts/tool.ts")),
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
