//! Registry, layout, and warm-up-document behaviour for the live backends.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rag_rat_base::language::Language;
use rag_rat_base::test_scratch::{self, ScratchDir};

use super::documents::enclosing_project_dir;
use super::registry::LiveBackend;
use crate::OracleTool;
use crate::test_support::every_path_scope as scope;

mod compdb;
mod markers;
mod registry;
mod warmup;

/// A scratch checkout, paired with the CANONICAL spelling of its root — the only spelling these
/// tests may build paths from.
///
/// [`CheckoutScope::resolve`](super::CheckoutScope::resolve) canonicalizes the root it is handed
/// (as `Config::load` does), so every path a layout, marker search or warm-up document comes back
/// as is spelled through that root. Scratch paths reach their directory through a symlinked
/// ancestor, so the guard's own spelling is a SECOND name for it — comparing against that name is
/// the divergence macOS (`/var` → `/private/var`) and Windows (8.3 `RUNNER~1`) hand over by
/// default, and it is why the guard is returned opaque here (#1027).
fn checkout(tag: &str) -> (ScratchDir, PathBuf) {
    let scratch = ScratchDir::new(tag);
    let root = test_scratch::canonical_config_root(scratch.path());
    (scratch, root)
}

/// The guard's spelling of a scratch root and the canonical one [`checkout`] hands back must be
/// two names for ONE directory. Without the divergence every assertion in this module would pass
/// whether it derived its paths from the canonical root or from the guard, and the root-spelling
/// class would only redden the cross-platform legs (#1027).
#[cfg(unix)]
#[test]
fn a_scratch_checkouts_canonical_root_diverges_from_the_guards_spelling() {
    let (guard, root) = checkout("scope-root-spelling");
    assert_ne!(root, guard.path(), "the guard must reach its root through a symlinked ancestor");
    assert_eq!(
        root,
        rag_rat_base::paths::canonicalize(guard.path()).unwrap(),
        "both spellings name one directory"
    );
    assert_eq!(scope(&root).root(), root, "and the scope resolves to the canonical one");
}

/// A compilation database naming `files`, written at `dir/relative`.
fn write_database(dir: &Path, relative: &str, files: &[&str]) {
    let entries: Vec<String> = files
        .iter()
        .map(|file| {
            let absolute = dir.join(file);
            format!(
                r#"{{"directory":{},"file":{},"command":"cc -c {}"}}"#,
                serde_json::to_string(&dir.to_string_lossy()).unwrap(),
                serde_json::to_string(&absolute.to_string_lossy()).unwrap(),
                file
            )
        })
        .collect();
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("[{}]", entries.join(","))).unwrap();
}

/// The `--compile-commands-dir` argument for `dir`, built the way production builds it.
fn compdb_arg(dir: &Path) -> OsString {
    let mut arg = OsString::from("--compile-commands-dir=");
    arg.push(dir.as_os_str());
    arg
}

/// A compilation database with one real entry. `[]` is syntactically valid but describes no
/// project, and clangd emits no readiness cycle for it — writing that in a fixture would
/// assert the very bug `marker_is_usable` exists to catch.
const COMPDB: &str = r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c"}]"#;

/// A TypeScript project at `relative_dir` holding one `main.ts`.
fn write_project(root: &Path, relative_dir: &str) {
    let dir = root.join(relative_dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
    std::fs::write(dir.join("main.ts"), "export function greet() {}\n").unwrap();
}
