//! Finding a document to warm a live session on: which project encloses a given file, and the two
//! whole-checkout searches for a warm-up document.
//!
//! Which directories a search may enter, and which files count as this checkout's own, are the
//! indexed corpus's answer — never a directory-name test. A name test cannot tell clangd's own
//! `.cache/clangd` index from a `.cache/generated` tree of real sources, and guessed wrong in both
//! directions (#1008, #1011).

use std::path::{Path, PathBuf};

use rag_rat_base::language::Language;

use super::scope::CheckoutScope;

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
pub(super) fn enclosing_project_dir(
    checkout: &CheckoutScope<'_>,
    path: &Path,
    markers: &[&str],
) -> Option<PathBuf> {
    let ceiling = checkout.ceiling();
    let mut dir = path.parent()?;
    loop {
        if markers.iter().any(|marker| dir.join(marker).exists()) {
            return Some(dir.to_path_buf());
        }
        if dir == ceiling {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// The first file one of `languages` claims at or below `dir` that also satisfies `accept`.
pub(super) fn find_document_where(
    checkout: &CheckoutScope<'_>,
    dir: &Path,
    languages: &[Language],
    accept: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut subdirectories = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            // The CORPUS prunes, not a name test. It refuses the machine-written trees the old
            // dot-rule caught (`.git`, `.rag-rat`, `node_modules`, `.cache/clangd`) through the
            // indexing floor, and it admits the hidden directories that hold real sources, which
            // the name test could not tell apart (#1011).
            if checkout.corpus().may_hold_indexed_files(&path) {
                subdirectories.push(path);
            }
        } else if languages.iter().any(|language| language.claims_path(&path))
            && checkout.corpus().indexes_file(&path)
            && accept(&path)
        {
            return Some(path);
        }
    }
    subdirectories.sort();
    subdirectories
        .into_iter()
        .find_map(|sub| find_document_where(checkout, &sub, languages, accept))
}

/// The first file one of `languages` claims that lies inside a `marker` project at or below `dir`.
///
/// Deliberately UNBOUNDED in depth (it only skips vendored/VCS directories): a project can sit
/// arbitrarily deep in a monorepo — `services/teams/foo/web/tsconfig.json` is an ordinary layout —
/// and a fixed depth limit would silently disable those checkouts. The walk early-exits on the
/// first hit, so the only full traversal is the case that ends in a `Blocked` verdict, which the
/// watcher then backs off to five-minute retries.
pub(super) fn find_document_in_project(
    checkout: &CheckoutScope<'_>,
    dir: &Path,
    languages: &[Language],
    markers: &[&str],
    inside_project: bool,
) -> Option<PathBuf> {
    let inside_project = inside_project || markers.iter().any(|marker| dir.join(marker).exists());
    let mut subdirectories = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            if checkout.corpus().may_hold_indexed_files(&path) {
                subdirectories.push(path);
            }
        } else if inside_project
            && languages.iter().any(|language| language.claims_path(&path))
            && checkout.corpus().indexes_file(&path)
        {
            return Some(path);
        }
    }
    subdirectories.into_iter().find_map(|sub| {
        find_document_in_project(checkout, &sub, languages, markers, inside_project)
    })
}
