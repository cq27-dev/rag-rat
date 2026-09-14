//! The indexed corpus as an answerable question: which files this checkout actually indexes.
//!
//! The live oracle needs this to decide whether a compilation database describes anything this
//! checkout owns (#1008) and to stop guessing at source locations from directory names (#1011).
//! It cannot derive it — the crate dependency runs core → oracle — so the oracle declares
//! [`IndexedCorpus`] and this module supplies the implementation.
//!
//! (Not to be confused with `rag_rat_oracle::corpus`, which is the eval-corpus registry — the
//! benchmark repositories an oracle run is scored against.)

use std::path::Path;

use rag_rat_base::config::{Config, ResolvedTarget};
use rag_rat_oracle::IndexedCorpus;

use super::discovery;
use super::ignore_rules::IgnoreMatcher;

/// The real corpus: resolved targets plus the compiled ignore rules — the same two authorities the
/// indexing walk consults, so "the live oracle thinks this file is ours" cannot drift from "the
/// indexer indexes it".
pub(crate) struct ConfiguredCorpus<'a> {
    config: &'a Config,
    ignore: IgnoreMatcher,
    /// `config.targets` pre-sorted by [`ResolvedTarget::index_precedence`].
    ///
    /// [`discovery::target_for_path`] sorts on every call, which is free at index time (once per
    /// walk) and is not free here: the governance read asks this question once per compilation
    /// database entry, and a large database carries 120k of them, while the maintenance pass holds
    /// the repository write lock.
    targets: Vec<&'a ResolvedTarget>,
}

impl ConfiguredCorpus<'_> {
    /// Whether a target walk could reach `relative` without crossing a symlink.
    ///
    /// The bound is the TARGET DIRECTORY, not the index root, because that is where the walk
    /// starts: `walk_target` enters its target with `is_dir()`, which FOLLOWS links, and only
    /// skips symlinked entries it meets while descending. So a symlinked target root is walked
    /// and its ordinary children are indexed — testing symlinks from the index root instead
    /// would reject exactly those files and declare a database that names them to govern
    /// nothing.
    fn reachable_by_a_target_walk(&self, relative: &Path) -> bool {
        self.targets.iter().flat_map(|target| target.directories.iter()).any(|dir| {
            let below = if dir.as_os_str().is_empty() || dir == Path::new(".") {
                Some(relative)
            } else {
                relative.strip_prefix(dir).ok()
            };
            below.is_some_and(|below| {
                !super::prep::path_crosses_symlink(&self.config.root.join(dir), below)
            })
        })
    }
}

impl<'a> ConfiguredCorpus<'a> {
    pub(crate) fn new(config: &'a Config) -> Self {
        let mut targets = config.targets.iter().collect::<Vec<_>>();
        targets.sort_by_key(|target| target.index_precedence());
        Self {
            config,
            ignore: IgnoreMatcher::compile(&config.root, &config.target_directories()),
            targets,
        }
    }
}

impl IndexedCorpus for ConfiguredCorpus<'_> {
    fn indexes_file(&self, absolute: &Path) -> bool {
        // Outside the root is outside the corpus, and `[index] root` containment is enforced when
        // the config loads, so no configured target can put an indexed file out here.
        let Ok(relative) = absolute.strip_prefix(&self.config.root) else {
            return false;
        };
        // Everything below mirrors what `walker::walk_target` would actually DO with this path.
        // Approximating it is how this predicate drifts from the thing it claims to be: a
        // compilation database whose only apparent corpus coverage is a path the indexer never
        // yields would be judged to govern the checkout, get pinned globally, and have its inferred
        // flags produce trusted definitions for the files that genuinely are indexed.
        //
        // The walker yields an entry only when `file_type().is_file()` — from `read_dir`, so it
        // does NOT follow links. A path that does not exist (a database left stale by a delete or
        // rename), a directory, and a symlink all fail that test.
        if !absolute.symlink_metadata().is_ok_and(|meta| meta.file_type().is_file()) {
            return false;
        }
        self.reachable_by_a_target_walk(relative)
            && !self.ignore.is_ignored(absolute, false)
            && discovery::target_claims_path(&self.targets, relative).is_some()
    }

    fn may_hold_indexed_files(&self, dir: &Path) -> bool {
        if self.ignore.is_ignored(dir, true) {
            return false;
        }
        // Outside the root nothing is indexed — `[index] root` containment is enforced at config
        // load — so such a directory holds no indexed file whatever the ignore rules say about it.
        let Ok(relative) = dir.strip_prefix(&self.config.root) else {
            return false;
        };
        // The ignore rules alone are not enough. With narrow targets (`src/` only), every unignored
        // directory would still be reported as possibly holding indexed files, so a large unbound
        // tree — the kind the blanket dot-directory rule used to skip for free — is now walked in
        // full by the warm-up search, while the maintenance pass holds the repository write lock.
        //
        // Both arms are needed: a directory INSIDE a target subtree can hold indexed files, and one
        // that is an ANCESTOR of a target must still be entered to reach it (the root itself
        // relativizes to the empty path, which every target starts with).
        self.targets.iter().flat_map(|target| target.directories.iter()).any(|target_dir| {
            // `.` and the empty path both spell "the whole root" — `push_target` keeps the former
            // for corpus-profile comparison, and `target_claims_path` accepts either.
            target_dir.as_os_str().is_empty()
                || target_dir == Path::new(".")
                || relative.starts_with(target_dir)
                || target_dir.starts_with(relative)
        })
    }
}

#[cfg(test)]
#[path = "corpus_tests.rs"]
mod tests;
