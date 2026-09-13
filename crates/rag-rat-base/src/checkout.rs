//! The checkout a scoped row belongs to: its `(commit_sha, worktree_id)` pair.
//!
//! Index rows, oracle verdicts and scope views are all keyed by this pair. Both halves are plain
//! strings, so passing them positionally lets a transposed pair compile and silently read another
//! checkout; these types name the halves at every seam that carries a scope.

/// An owned checkout key: what scope resolution returns and long-lived contexts hold.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CheckoutKey {
    pub commit_sha: String,
    pub worktree_id: String,
}

impl CheckoutKey {
    /// The borrowed form scoped queries take.
    pub fn borrowed(&self) -> CheckoutRef<'_> {
        CheckoutRef { commit_sha: &self.commit_sha, worktree_id: &self.worktree_id }
    }
}

/// A borrowed checkout key: how a scope is passed down to the queries that read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CheckoutRef<'a> {
    pub commit_sha: &'a str,
    pub worktree_id: &'a str,
}
