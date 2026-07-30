use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use rag_rat_base::config::ResolvedTarget;

use crate::index::ignore_rules::IgnoreMatcher;

/// Walk one target's directories, honoring the repo's compiled `.gitignore` rules (root + nested)
/// plus the hardcoded floor (see [`IgnoreMatcher`]). `ignore` is compiled once per index pass and
/// shared across targets so the walker and watcher classify paths identically (issue #62).
pub fn walk_target(
    root: &Path,
    target: &ResolvedTarget,
    ignore: &IgnoreMatcher,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = BTreeSet::new();
    for directory in &target.directories {
        let dir = root.join(directory);
        // A configured target directory may be ABSENT under `root` — most importantly when the
        // config came from a linked worktree that added a BRANCH-ONLY target dir, but `Config.root`
        // is anchored to the MAIN checkout (so the shared index has one base commit) (#219 review).
        // Base discovery over main must SKIP the missing dir, not hard-error: there is nothing to
        // index there on main, and the linked worktree's overlay pass picks up the branch-only
        // files separately. Without this, a hook/maintenance launched from such a branch
        // aborted before `refresh_worktree_overlays` could run.
        if !dir.is_dir() {
            continue;
        }
        walk_dir(root, &dir, target, ignore, &mut files)?;
    }
    Ok(files.into_iter().collect())
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    target: &ResolvedTarget,
    ignore: &IgnoreMatcher,
    files: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        // Honor `.gitignore` (root + nested) and the hardcoded floor. We test the full path with
        // its dir-ness so nested-gitignore scoping and `foo/`-style dir-only rules resolve
        // correctly; the floor (`.git`, `.rag-rat`, `target`, …) short-circuits inside
        // `is_ignored`.
        if ignore.is_ignored(&path, file_type.is_dir()) {
            continue;
        }
        if file_type.is_dir() {
            walk_dir(root, &path, target, ignore, files)?;
        } else if file_type.is_file() && is_target_file(root, &path, target) {
            files.insert(path);
        }
    }
    Ok(())
}

fn is_target_file(root: &Path, path: &Path, target: &ResolvedTarget) -> bool {
    // The target's language must CLAIM this extension (not bare `from_path` detection): that's what
    // lets a `cpp` binding index its `.h` headers, which bare detection resolves to C. The file is
    // then parsed as the target's language (see `discovery::target_for_path`).
    if !target.language.claims_path(path) {
        return false;
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    // The include/exclude patterns are `/`-spelled, so match against the SAME rendering
    // `files.path` is stored with — never a blanket backslash rewrite, which would let a pattern
    // claim (or exclude) a Unix file whose NAME contains a backslash by pretending it is nested.
    let relative = rag_rat_base::paths::path_string(relative);
    target.globs_claim(&relative)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rag_rat_base::config::TargetKind;
    use rag_rat_base::language::Language;

    use super::*;

    fn rust_target() -> ResolvedTarget {
        ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from(".")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn tempdir() -> rag_rat_base::test_scratch::ScratchDir {
        rag_rat_base::test_scratch::ScratchDir::new("walk")
    }

    fn cpp_target() -> ResolvedTarget {
        ResolvedTarget {
            name: "cpp".to_string(),
            language: Language::Cpp,
            directories: vec![PathBuf::from(".")],
            include: Language::Cpp.default_include_globs(),
            exclude: Vec::new(),
            kind: TargetKind::Source,
        }
    }

    #[test]
    fn cpp_target_indexes_h_headers_but_not_c_sources() {
        let root = tempdir();
        write(&root.join("include/lib.h"), "class C { void f(); };\n");
        write(&root.join("src/lib.cpp"), "#include \"lib.h\"\nvoid C::f() {}\n");
        write(&root.join("legacy.c"), "int g(void){return 0;}\n");

        let target = cpp_target();
        let ignore = IgnoreMatcher::compile(&root, &target.directories);
        let rel: Vec<String> = walk_target(&root, &target, &ignore)
            .unwrap()
            .iter()
            .map(|p| rag_rat_base::paths::path_string(p.strip_prefix(&root).unwrap()))
            .collect();

        // The `.h` header is claimed by the cpp binding (the header-resolution fix)...
        assert!(rel.contains(&"include/lib.h".to_string()), "cpp must claim .h: {rel:?}");
        assert!(rel.contains(&"src/lib.cpp".to_string()), "{rel:?}");
        // ...but a plain `.c` file is NOT a C++ source.
        assert!(!rel.contains(&"legacy.c".to_string()), "cpp must not claim .c: {rel:?}");
    }

    #[test]
    fn walk_skips_gitignored_and_nested_gitignored_files() {
        let root = tempdir();
        // Root gitignore hides `generated/`; a nested gitignore hides `skip.rs` under `crates/app`.
        write(&root.join(".gitignore"), "generated/\n");
        write(&root.join("crates/app/.gitignore"), "skip.rs\n");
        write(&root.join("crates/app/keep.rs"), "fn a() {}\n");
        write(&root.join("crates/app/skip.rs"), "fn b() {}\n");
        write(&root.join("generated/out.rs"), "fn c() {}\n");
        // A floor dir (target/) must be skipped even without a gitignore entry for it.
        write(&root.join("target/debug/built.rs"), "fn d() {}\n");
        // A sibling named `skip.rs` at the root is NOT covered by the nested gitignore.
        write(&root.join("skip.rs"), "fn e() {}\n");

        let target = rust_target();
        let ignore = IgnoreMatcher::compile(&root, &target.directories);
        let mut found = walk_target(&root, &target, &ignore).unwrap();
        found.sort();
        let rel: Vec<String> = found
            .iter()
            .map(|p| rag_rat_base::paths::path_string(p.strip_prefix(&root).unwrap()))
            .collect();

        assert!(rel.contains(&"crates/app/keep.rs".to_string()), "kept: {rel:?}");
        assert!(rel.contains(&"skip.rs".to_string()), "root skip.rs not nested-ignored: {rel:?}");
        assert!(!rel.contains(&"crates/app/skip.rs".to_string()), "nested gitignore: {rel:?}");
        assert!(!rel.contains(&"generated/out.rs".to_string()), "root gitignore: {rel:?}");
        assert!(!rel.iter().any(|p| p.starts_with("target/")), "floor dir: {rel:?}");
    }

    #[test]
    fn walk_skips_a_missing_target_directory() {
        // #219 review: a config carrying a BRANCH-ONLY target dir is anchored to the MAIN root, so
        // base discovery over main must SKIP the absent dir, not hard-error on `read_dir` — else a
        // hook/maintenance launched from such a branch aborts before the overlay pass can run.
        let root = tempdir();
        write(&root.join("present/lib.rs"), "fn a() {}\n");
        let target = ResolvedTarget {
            name: "rust".to_string(),
            language: Language::Rust,
            directories: vec![PathBuf::from("present"), PathBuf::from("branch_only")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: TargetKind::Source,
        };
        let ignore = IgnoreMatcher::compile(&root, &target.directories);

        let found = walk_target(&root, &target, &ignore).expect("missing dir must not error");
        let rel: Vec<String> = found
            .iter()
            .map(|p| rag_rat_base::paths::path_string(p.strip_prefix(&root).unwrap()))
            .collect();
        assert_eq!(rel, vec!["present/lib.rs".to_string()], "present dir indexed, missing skipped");
    }

    /// Unix-only: Windows forbids `\` in a file name, so the fixture cannot exist there.
    ///
    /// Preserving the backslash in the rendering accomplishes nothing if the exclude matcher then
    /// claims the file for a directory it is not in. `drafts/**` used to `starts_with("drafts")`,
    /// so a real source file NAMED `drafts\secret.rs` was dropped from the walk entirely.
    #[cfg(unix)]
    #[test]
    fn a_directory_exclude_does_not_claim_a_backslash_named_file() {
        let root = tempdir();
        write(&root.join("drafts/secret.rs"), "fn inside_drafts() {}\n");
        write(&root.join("drafts\\secret.rs"), "fn named_with_a_backslash() {}\n");
        write(&root.join("draftsman.rs"), "fn a_longer_name() {}\n");

        let target = ResolvedTarget { exclude: vec!["drafts/**".to_string()], ..rust_target() };
        let ignore = IgnoreMatcher::compile(&root, &target.directories);
        let rel: Vec<String> = walk_target(&root, &target, &ignore)
            .unwrap()
            .iter()
            .map(|p| rag_rat_base::paths::path_string(p.strip_prefix(&root).unwrap()))
            .collect();

        assert!(
            !rel.contains(&"drafts/secret.rs".to_string()),
            "a real child is excluded: {rel:?}"
        );
        assert!(
            rel.contains(&"drafts\\secret.rs".to_string()),
            "a file NAMED `drafts\\secret.rs` is not under drafts/: {rel:?}"
        );
        assert!(rel.contains(&"draftsman.rs".to_string()), "a name prefix is not a dir: {rel:?}");
    }
}
