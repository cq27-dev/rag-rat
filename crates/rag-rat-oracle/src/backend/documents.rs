//! Finding a document to warm a live session on: which directories a document search may enter,
//! which project encloses a given file, and the two whole-checkout searches for a warm-up document.

use std::path::{Path, PathBuf};

use rag_rat_base::language::Language;

/// Directories never searched for a warm-up DOCUMENT: `node_modules` vendors dependency sources,
/// and a dot-directory is VCS/tooling state (including the `.cache/clangd` index clangd writes
/// itself) — never somewhere to pick a document this checkout owns.
fn is_searchable_dir(name: &str) -> bool {
    name != "node_modules" && !name.starts_with('.')
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
pub(super) fn enclosing_project_dir(root: &Path, path: &Path, marker: &str) -> Option<PathBuf> {
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

/// The first file one of `languages` claims at or below `dir` that also satisfies `accept`.
pub(super) fn find_document_where(
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
pub(super) fn find_document_in_project(
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
