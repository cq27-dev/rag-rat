//! A distill thread's full identity, shared by extraction and the drain, plus the thread-keyed
//! writers both halves call.
//!
//! Issue/PR numbers are unique only per `(tracker, project)` — one repo can mirror several tracker
//! bindings — so every thread-keyed distill row is addressed by all four of
//! `(tracker, project, item_kind, item_key)` under its `repo_id`.

use rusqlite::Connection;

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

/// Which thread a snapshotted source belongs to (`papertrail_distill_sources.role`): the record's
/// own thread or a coalesced partner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceRole {
    Primary,
    Partner,
}

impl SourceRole {
    pub(super) fn as_db_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Partner => "partner",
        }
    }

    pub(super) fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "primary" => Ok(Self::Primary),
            "partner" => Ok(Self::Partner),
            other => anyhow::bail!("unknown distill source role `{other}`"),
        }
    }
}

/// What a snapshotted source is (`papertrail_distill_sources.source_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceKind {
    Item,
    Comment,
}

impl SourceKind {
    pub(super) fn as_db_str(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Comment => "comment",
        }
    }

    pub(super) fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "item" => Ok(Self::Item),
            "comment" => Ok(Self::Comment),
            other => anyhow::bail!("unknown distill source kind `{other}`"),
        }
    }
}

/// Which part of its item a snapshotted source holds (`papertrail_distill_sources.source_part`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourcePart {
    Title,
    Body,
    Comment,
}

impl SourcePart {
    pub(super) fn as_db_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Body => "body",
            Self::Comment => "comment",
        }
    }

    pub(super) fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "title" => Ok(Self::Title),
            "body" => Ok(Self::Body),
            "comment" => Ok(Self::Comment),
            other => anyhow::bail!("unknown distill source part `{other}`"),
        }
    }
}

/// Delete the model-owned junction rows (evidence, alternatives) of one thread.
pub(crate) fn clear_model_junctions(
    conn: &Connection,
    repo_id: &str,
    key: &ThreadKey,
) -> anyhow::Result<()> {
    for table in ["papertrail_distill_evidence", "papertrail_distill_alternatives"] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE {THREAD_KEY_WHERE}"),
            key.params(repo_id),
        )?;
    }
    Ok(())
}

/// Clear the model's anchor selections on one thread, keeping the mined candidates themselves.
pub(crate) fn deselect_anchors(
    conn: &Connection,
    repo_id: &str,
    key: &ThreadKey,
) -> anyhow::Result<()> {
    conn.execute(
        &format!("UPDATE papertrail_distill_anchors SET selected = 0 WHERE {THREAD_KEY_WHERE}"),
        key.params(repo_id),
    )?;
    Ok(())
}
