use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ResolvedTarget;
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
        walk_dir(root, &root.join(directory), target, ignore, &mut files)?;
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
    let Some(language) = crate::language::Language::from_path(path) else {
        return false;
    };
    if language != target.language {
        return false;
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    let relative = relative.to_string_lossy().replace('\\', "/");
    if target.exclude.iter().any(|pattern| matches_simple_pattern(&relative, pattern)) {
        return false;
    }
    target.include.iter().any(|pattern| matches_simple_pattern(&relative, pattern))
}

fn matches_simple_pattern(path: &str, pattern: &str) -> bool {
    if let Some(extension) = pattern.strip_prefix("**/*.") {
        return path.ends_with(&format!(".{extension}"));
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path.starts_with(prefix);
    }
    path == pattern || path.contains(pattern.trim_matches('*'))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::config::TargetKind;
    use crate::language::Language;

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

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ragrat-walk-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
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
            .map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"))
            .collect();

        assert!(rel.contains(&"crates/app/keep.rs".to_string()), "kept: {rel:?}");
        assert!(rel.contains(&"skip.rs".to_string()), "root skip.rs not nested-ignored: {rel:?}");
        assert!(!rel.contains(&"crates/app/skip.rs".to_string()), "nested gitignore: {rel:?}");
        assert!(!rel.contains(&"generated/out.rs".to_string()), "root gitignore: {rel:?}");
        assert!(!rel.iter().any(|p| p.starts_with("target/")), "floor dir: {rel:?}");

        fs::remove_dir_all(&root).ok();
    }
}
