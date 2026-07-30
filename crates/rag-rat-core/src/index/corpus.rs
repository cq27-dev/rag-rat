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
mod tests {
    use super::*;

    /// A checkout with `src` bound as a Rust target, a gitignored `generated/`, and the usual
    /// machine-written trees.
    ///
    /// Assert against `config.root`, NEVER the guard's own spelling. `Config::load` canonicalizes
    /// its root and [`ConfiguredCorpus`] strips exactly that root off the path it is asked about,
    /// while a scratch path reaches its directory through a symlinked ancestor — so a test holding
    /// the guard's spelling asks about a root production never produces, and every answer comes
    /// back `false` for the wrong reason. Non-canonical temp roots are what macOS (`/var` →
    /// `/private/var`) and Windows (8.3 `RUNNER~1`) hand over by default (#1027).
    fn fixture(tag: &str) -> (rag_rat_base::test_scratch::ScratchDir, Config) {
        let dir = rag_rat_base::test_scratch::ScratchDir::new(tag);
        for relative in ["src", "vendor", "node_modules", "generated", "build"] {
            std::fs::create_dir_all(dir.join(relative)).unwrap();
        }
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("vendor/dep.rs"), "pub fn dep() {}\n").unwrap();
        std::fs::write(dir.join("generated/out.rs"), "pub fn out() {}\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(
            dir.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \
             \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        let config = Config::load(dir.join("rag-rat.toml")).unwrap();
        (dir, config)
    }

    /// The guard's spelling of the root and `config.root` must be two names for ONE directory —
    /// the shape `Config::load` produces wherever the system temp is reached through a symlink
    /// (macOS `/var` → `/private/var`) or an 8.3 alias (Windows `RUNNER~1`). Without the
    /// divergence every assertion in this module would pass whether it derived its paths from
    /// `config.root` or from the guard, and the root-spelling class would only redden the
    /// cross-platform legs (#1027).
    #[cfg(unix)]
    #[test]
    fn the_fixture_config_root_diverges_from_its_scratch_spelling() {
        let (scratch, config) = fixture("corpus-root-spelling");
        assert_ne!(
            config.root,
            scratch.path(),
            "the fixture must reach its root through a symlinked ancestor",
        );
        assert_eq!(
            config.root,
            scratch.path().canonicalize().unwrap(),
            "both spellings name the same directory",
        );
    }

    /// Corpus membership answers about the CONFIG-ROOT spelling and no other, and an alias of an
    /// indexed file is refused rather than resolved.
    ///
    /// Deliberate: this predicate mirrors what `walk_target` yields, and the walk yields paths
    /// under the canonical root. Resolving an alias here would have to canonicalize the path's
    /// ancestors, which is exactly what the symlink rules above must see un-resolved — a file
    /// under a symlinked ancestor would come back rebased onto its target and be admitted. The
    /// caller's fallback for a refusal is to treat a compilation database as governing nothing,
    /// which declines analysis rather than trusting the wrong flags (#1008).
    #[cfg(unix)]
    #[test]
    fn an_aliased_spelling_of_an_indexed_file_is_refused_not_resolved() {
        let (scratch, config) = fixture("corpus-aliased-spelling");
        let corpus = ConfiguredCorpus::new(&config);

        assert!(corpus.indexes_file(&config.root.join("src/main.rs")), "the control");
        assert!(
            !corpus.indexes_file(&scratch.join("src/main.rs")),
            "the same file spelled through a symlinked ancestor is not answered for",
        );
    }

    #[test]
    fn a_bound_source_is_in_the_corpus_and_an_unbound_sibling_is_not() {
        let (_scratch, config) = fixture("corpus-bound-target");
        let root = config.root.clone();
        let corpus = ConfiguredCorpus::new(&config);

        assert!(corpus.indexes_file(&root.join("src/main.rs")), "a bound target's source");
        assert!(
            !corpus.indexes_file(&root.join("vendor/dep.rs")),
            "a Rust file no target binds is not this checkout's corpus — which is exactly the \
             vendored-database case (#1008)",
        );
    }

    /// A path reached through a symlink is not in the corpus, because the indexer does not index
    /// it.
    ///
    /// `walker::walk_dir` skips any entry crossing a symlink at any component, so such a file has
    /// no indexed row. Without the same rule here, a compilation database whose only apparent
    /// corpus coverage is a symlinked path would be judged to govern the checkout, get pinned
    /// globally, and have its inferred flags produce trusted definitions for the files that
    /// genuinely are indexed.
    #[cfg(unix)]
    #[test]
    fn a_source_reached_through_a_symlink_is_not_in_the_corpus() {
        let (_scratch, config) = fixture("corpus-symlinked-source");
        let root = config.root.clone();
        std::fs::write(root.join("vendor/real.rs"), "pub fn real() {}\n").unwrap();
        std::os::unix::fs::symlink(root.join("vendor/real.rs"), root.join("src/linked.rs"))
            .unwrap();
        std::os::unix::fs::symlink(root.join("vendor"), root.join("src/linked_dir")).unwrap();
        let corpus = ConfiguredCorpus::new(&config);

        assert!(corpus.indexes_file(&root.join("src/main.rs")), "the control: a real bound source");
        assert!(
            !corpus.indexes_file(&root.join("src/linked.rs")),
            "a symlinked LEAF under a bound target is not indexed, so it is not in the corpus",
        );
        assert!(
            !corpus.indexes_file(&root.join("src/linked_dir/real.rs")),
            "nor is a path crossing a symlinked ancestor",
        );
    }

    /// A stale entry — a path deleted or renamed since the database was generated — establishes
    /// nothing. The walker yields only regular files that exist; a database whose apparent corpus
    /// coverage is a path that is gone would otherwise be pinned globally and have that obsolete
    /// entry's flags inferred for the files that do exist.
    #[test]
    fn a_path_that_is_not_a_regular_file_is_not_in_the_corpus() {
        let (_scratch, config) = fixture("corpus-not-a-regular-file");
        let root = config.root.clone();
        let corpus = ConfiguredCorpus::new(&config);

        assert!(corpus.indexes_file(&root.join("src/main.rs")), "the control: a real bound source");
        assert!(
            !corpus.indexes_file(&root.join("src/deleted.rs")),
            "a path the database still names but the checkout no longer has",
        );
        std::fs::create_dir_all(root.join("src/looks_like.rs")).unwrap();
        assert!(
            !corpus.indexes_file(&root.join("src/looks_like.rs")),
            "a DIRECTORY with a source-shaped name is not a translation unit either",
        );
    }

    /// A target directory that is ITSELF a symlink is walked — `walk_target` enters it with
    /// `is_dir()`, which follows links — so its ordinary children are indexed and belong to the
    /// corpus. Testing symlinks from the index root instead would reject exactly those files and
    /// declare a database naming them to govern nothing, disabling live analysis for a checkout
    /// that indexes fine.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_target_root_is_walked_so_its_children_are_in_the_corpus() {
        let dir = rag_rat_base::test_scratch::ScratchDir::new("corpus-symlinked-target-root");
        std::fs::create_dir_all(dir.join("real_sources")).unwrap();
        std::fs::write(dir.join("real_sources/main.rs"), "pub fn real() {}\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real_sources"), dir.join("src")).unwrap();
        std::fs::write(
            dir.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \
             \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = [\"src\"]\n",
        )
        .unwrap();
        let config = Config::load(dir.join("rag-rat.toml")).unwrap();
        let corpus = ConfiguredCorpus::new(&config);

        assert!(
            corpus.indexes_file(&config.root.join("src/main.rs")),
            "the walk starts AT the target, so the target's own link is not a crossing",
        );
    }

    #[test]
    fn a_gitignored_source_is_not_in_the_corpus() {
        let (_scratch, config) = fixture("corpus-gitignored");
        let corpus = ConfiguredCorpus::new(&config);

        assert!(
            corpus.indexes_file(&config.root.join("src/main.rs")),
            "the control: without it the gitignore assertion below would also hold for a path the \
             corpus simply failed to recognize",
        );
        assert!(!corpus.indexes_file(&config.root.join("generated/out.rs")));
    }

    /// The floor removes `.cache/clangd` SPECIFICALLY, not all of `.cache` — so a checkout whose
    /// sources sit under another hidden directory is indexed, and the live oracle's document search
    /// (which now asks this same corpus) can warm on them (#1011).
    ///
    /// This is the half a test double cannot establish: it needs the real floor.
    #[test]
    fn the_floor_removes_clangds_index_without_removing_the_rest_of_dot_cache() {
        let dir = rag_rat_base::test_scratch::ScratchDir::new("corpus-dot-cache");
        for relative in [".cache/generated", ".cache/clangd"] {
            std::fs::create_dir_all(dir.join(relative)).unwrap();
        }
        std::fs::write(dir.join(".cache/generated/gen.rs"), "pub fn gen() {}\n").unwrap();
        std::fs::write(dir.join(".cache/clangd/stale.rs"), "pub fn stale() {}\n").unwrap();
        std::fs::write(
            dir.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \
             \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = [\".cache/generated\"]\n",
        )
        .unwrap();
        let config = Config::load(dir.join("rag-rat.toml")).unwrap();
        let root = config.root.clone();
        let corpus = ConfiguredCorpus::new(&config);

        assert!(
            corpus.indexes_file(&root.join(".cache/generated/gen.rs")),
            "a hidden directory bound as a target is an ordinary source location",
        );
        assert!(
            !corpus.may_hold_indexed_files(&root.join(".cache/clangd")),
            "clangd's own index never holds this checkout's source",
        );
    }

    /// A directory no target can reach holds no indexed file, and saying so is what keeps the
    /// warm-up search from walking a large unbound tree under the repository write lock.
    ///
    /// The ignore rules alone do not answer this: an unignored, unbound `.cache/` is not ignored,
    /// and it used to be skipped for free by the blanket dot-directory rule the corpus replaced.
    /// Ancestors of a target must still be admitted, or the walk could never reach the target.
    #[test]
    fn a_directory_no_target_can_reach_holds_no_indexed_files() {
        let dir = rag_rat_base::test_scratch::ScratchDir::new("corpus-unreachable-dirs");
        for relative in ["src", "deep/nested/gen", ".cache/big"] {
            std::fs::create_dir_all(dir.join(relative)).unwrap();
        }
        std::fs::write(
            dir.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \
             \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = [\"src\", \
             \"deep/nested/gen\"]\n",
        )
        .unwrap();
        let config = Config::load(dir.join("rag-rat.toml")).unwrap();
        let root = config.root.clone();
        let corpus = ConfiguredCorpus::new(&config);

        assert!(corpus.may_hold_indexed_files(&root), "the root is on the way to every target");
        assert!(corpus.may_hold_indexed_files(&root.join("src")), "a target subtree");
        assert!(
            corpus.may_hold_indexed_files(&root.join("deep")),
            "an ancestor of a target must be entered to reach it",
        );
        assert!(corpus.may_hold_indexed_files(&root.join("deep/nested/gen")));
        assert!(
            !corpus.may_hold_indexed_files(&root.join(".cache/big")),
            "unignored but unreachable by any target — no indexed file can live here",
        );
    }

    /// A whole-root binding admits every unignored directory — `.` and the empty path are the same
    /// statement, and the reachability check must read both.
    #[test]
    fn a_whole_root_target_admits_every_unignored_directory() {
        let dir = rag_rat_base::test_scratch::ScratchDir::new("corpus-dot-binding");
        std::fs::create_dir_all(dir.join("anywhere/deep")).unwrap();
        std::fs::write(
            dir.join("rag-rat.toml"),
            "[index]\nroot = \".\"\ndatabase = \
             \".rag-rat/index.sqlite\"\n\n[target_bindings]\nrust = [\".\"]\n",
        )
        .unwrap();
        let config = Config::load(dir.join("rag-rat.toml")).unwrap();
        let root = config.root.clone();
        let corpus = ConfiguredCorpus::new(&config);

        assert!(corpus.may_hold_indexed_files(&root.join("anywhere/deep")));
        assert!(
            !corpus.may_hold_indexed_files(&root.join("node_modules")),
            "the floor still applies"
        );
    }

    #[test]
    fn a_floored_directory_holds_no_indexed_files() {
        let (_scratch, config) = fixture("corpus-floored-dirs");
        let root = config.root.clone();
        let corpus = ConfiguredCorpus::new(&config);

        assert!(corpus.may_hold_indexed_files(&root.join("src")), "an ordinary source directory");
        assert!(
            !corpus.may_hold_indexed_files(&root.join("node_modules")),
            "vendored dependencies"
        );
        assert!(
            !corpus.may_hold_indexed_files(&root.join("build")),
            "a build tree is floored — which is why the MARKER search must not use this \
             predicate: it is where compilation databases live",
        );
    }
}
