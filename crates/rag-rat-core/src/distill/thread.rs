//! A distill thread's full identity, shared by extraction and the drain.
//!
//! Issue/PR numbers are unique only per `(tracker, project)` — one repo can mirror several tracker
//! bindings — so every thread-keyed distill row is addressed by all four of
//! `(tracker, project, item_kind, item_key)` under its `repo_id`.

/// A distill record's full thread identity within one repo.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ThreadKey {
    pub(crate) tracker: String,
    pub(crate) project: String,
    pub(crate) item_kind: String,
    pub(crate) item_key: String,
}

/// The `WHERE` predicate selecting one thread's rows in a thread-keyed distill table. Bind it with
/// [`ThreadKey::params`]: `?1` is the repo id, `?2..=?5` the key.
pub(crate) const THREAD_KEY_WHERE: &str =
    "repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5";

impl ThreadKey {
    /// The five bind values for [`THREAD_KEY_WHERE`], in placeholder order.
    pub(crate) fn params<'a>(&'a self, repo_id: &'a str) -> rusqlite::ParamsFromIter<[&'a str; 5]> {
        rusqlite::params_from_iter([
            repo_id,
            self.tracker.as_str(),
            self.project.as_str(),
            self.item_kind.as_str(),
            self.item_key.as_str(),
        ])
    }
}
