//! What a live backend may look at in a checkout, and what counts as the checkout's own source.
//!
//! One path — the configured `[index] root` — used to answer three different questions at once:
//! how far a filesystem walk may reach, where an ancestor walk stops, and which files this
//! checkout owns. Conflating them is what let a compilation database from an unindexed subtree be
//! pinned for first-party sources (#1008) and what made the warm-up document search guess at
//! ownership from directory names (#1011). They are separated here.

use std::path::{Path, PathBuf};

/// Whether a path belongs to the checkout's indexed corpus.
///
/// INJECTED rather than derived. The corpus is the resolved targets plus the ignore rules, which
/// `rag-rat-core` owns, and the crate dependency runs core → oracle. Re-deriving it here would be
/// a second source of truth for "what is this checkout's source", which is the bug class this
/// trait exists to remove — not a detail of how it is passed.
pub trait IndexedCorpus {
    /// Whether the file at `absolute` is one this checkout indexes.
    fn indexes_file(&self, absolute: &Path) -> bool;

    /// Whether any indexed file could live at or below `dir`. Answered for a DIRECTORY so a search
    /// can prune a subtree instead of descending it — the difference between skipping
    /// `node_modules` and walking all of it to reject each file.
    fn may_hold_indexed_files(&self, dir: &Path) -> bool;
}

/// The three boundaries a live backend needs, resolved together so they cannot drift apart.
pub struct CheckoutScope<'a> {
    /// The configured `[index] root`: where the marker descent starts, and what repo-relative
    /// paths are relative to.
    root: PathBuf,
    /// The enclosing checkout/worktree root — the stop of every ancestor walk and the bound a
    /// filesystem walk may not cross. Equal to `root` when `root` is not inside a git checkout.
    ceiling: PathBuf,
    corpus: &'a dyn IndexedCorpus,
}

impl<'a> CheckoutScope<'a> {
    /// Resolve the scope for `root`.
    ///
    /// The ceiling is DERIVED here, never passed in: it is a property of the filesystem, and a
    /// caller allowed to supply it would eventually supply the index root again — which is the
    /// conflation this type exists to remove.
    /// `root` is canonicalized so the two boundaries are comparable: the ceiling comes back
    /// canonical, and an ancestor walk from an uncanonical root would never recognise it as the
    /// stop. In production this is a no-op — a configured root is already canonicalized when the
    /// config loads — so it costs nothing and removes a class of mismatch from every test fixture
    /// and every checkout reached through a symlink.
    pub fn resolve(root: &Path, corpus: &'a dyn IndexedCorpus) -> Self {
        let root = rag_rat_base::paths::canonicalize_or_simplified(root);
        // The CONTAINING checkout, never the main one: each linked worktree is a different source
        // tree, so a ceiling pointing at main would admit another checkout's databases and
        // ancestors. `config::main_worktree_root` is the adjacent function that must NOT be used
        // here.
        //
        // The containment filter is what makes every remaining failure mode collapse to today's
        // behaviour: outside a git checkout, in a bare repository, or when the discovered workdir
        // is not an ancestor of the root at all, the root is its own ceiling and nothing widens.
        let ceiling = rag_rat_base::config::worktree_root(&root)
            .filter(|ceiling| root.starts_with(ceiling))
            .unwrap_or_else(|| root.clone());
        Self { root, ceiling, corpus }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ceiling(&self) -> &Path {
        &self.ceiling
    }

    pub fn corpus(&self) -> &dyn IndexedCorpus {
        self.corpus
    }
}
